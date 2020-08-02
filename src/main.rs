use std::collections::HashMap;
use std::error::Error;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::stream::StreamExt;

use chrono::{DateTime, NaiveDateTime, Utc};
use dns_lookup::lookup_addr;
use futures::future::join_all;
use futures::stream::FuturesOrdered;
use futures_batch::ChunksTimeoutStreamExt;
use lazy_static::lazy_static;
use listenfd::ListenFd;
use log::{debug, warn};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tantivy::schema::{Schema, SchemaBuilder, FAST, INDEXED, STORED, STRING, TEXT};
use tantivy::space_usage::SearcherSpaceUsage;
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task;
use tokio::time::delay_for;
use toshi::{Client, HyperToshi, IndexOptions};

mod gelf;
mod query;
mod syslog;
mod websrv;

type JsonMap<'a> = HashMap<&'a str, Value>;
type JsonOwnedKeysMap = HashMap<String, Value>;
type DefaultHyperToshi =
    HyperToshi<hyper::client::HttpConnector<hyper::client::connect::dns::GaiResolver>>;
type SharedSettings = Arc<Mutex<Settings>>;

pub struct Settings {
    pub toshi_url: String,
    pub index: u32,
    pub max_docs: u32,
}

impl Settings {
    fn index_name(&self) -> String {
        make_index_name(self.index)
    }
}

fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    env_logger::init();
    let mut rt = tokio::runtime::Runtime::new().unwrap();

    let toshi_url = "http://localhost:8080".to_string();
    let max_docs = 1_000_000;
    let index = rt.block_on(find_index(&toshi_url, max_docs))?;
    debug!("Found index name yaffle_{}", index);
    let settings = Arc::new(Mutex::new(Settings {
        toshi_url,
        index,
        max_docs,
    }));

    let mut listenfd = ListenFd::from_env();

    // Listen for GELF messages
    let (gelf_tx, gelf_rx) = mpsc::channel(10);
    let gelf_sock = if let Some(std_sock) = listenfd.take_udp_socket(0).ok().flatten() {
        debug!("Using passed GELF UDP socket {:?}", std_sock);
        rt.block_on(async { UdpSocket::from_std(std_sock) })?
    } else {
        debug!("Binding to [::]:12201 for GELF UDP");
        rt.block_on(UdpSocket::bind("[::]:12201"))?
    };

    rt.spawn(async { gelf::run_recv_loop(gelf_sock, gelf_tx).await.unwrap() });

    // Listen for Syslog messages
    let (syslog_tx, syslog_rx) = mpsc::channel(10);
    let syslog_sock = if let Some(std_sock) = listenfd.take_udp_socket(1).ok().flatten() {
        debug!("Using passed Syslog UDP socket {:?}", std_sock);
        rt.block_on(async { UdpSocket::from_std(std_sock) })?
    } else {
        debug!("Binding to [::]:10514 for Syslog UDP");
        rt.block_on(UdpSocket::bind("[::]:10514"))?
    };

    rt.spawn(async { syslog::run_recv_loop(syslog_sock, syslog_tx).await.unwrap() });

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    // Create HTTP listener and then leak it for the remainder of the program's lifetime
    let http_listener: &'static mut TcpListener = Box::leak(Box::new(
        if let Some(std_sock) = listenfd.take_tcp_listener(2).ok().flatten() {
            debug!("Using passed HTTP socket {:?}", std_sock);
            rt.block_on(async { TcpListener::from_std(std_sock) })?
        } else {
            debug!("Binding to [::]:8088 for HTTP");
            rt.block_on(TcpListener::bind("[::]:8088"))?
        },
    ));

    rt.spawn(index_manager(settings.clone()));
    rt.spawn(websrv::run_http_server(
        settings.clone(),
        http_listener,
        shutdown_rx,
    ));
    rt.block_on(async { async_dual_main(settings, gelf_rx, syslog_rx).await.unwrap() });
    shutdown_tx.send(()).ok();

    Ok(())
}

fn make_index_name(index: u32) -> String {
    format!("yaffle_{}", index)
}

/// A response gotten from the _summary route for an index
#[derive(Debug, Serialize, Deserialize)]
struct SummaryResponse {
    summaries: JsonOwnedKeysMap,
    #[serde(skip_serializing_if = "Option::is_none")]
    segment_sizes: Option<SearcherSpaceUsage>,
}

fn index_is_open(summary: &SummaryResponse, max_docs: u32) -> bool {
    summary
        .segment_sizes
        .as_ref()
        .map(|sizes| {
            sizes
                .segments()
                .iter()
                .fold(0, |acc, segment| acc + segment.num_docs())
                < max_docs
        })
        .unwrap_or(false)
}

async fn check_index(
    c: &DefaultHyperToshi,
    index: u32,
    max_docs: u32,
) -> Result<(bool, bool, u32), Box<dyn Error + Send + Sync>> {
    let index_name = make_index_name(index);
    let body = hyper::body::to_bytes(c.index_summary(&index_name, true).await?.into_body()).await?;
    serde_json::from_slice(&body)
        .map(|summary: SummaryResponse| {
            let is_open = index_is_open(&summary, max_docs);
            debug!(
                "Index {} {} open",
                index_name,
                if is_open { "is" } else { "is not" }
            );
            Ok((true, is_open, index))
        })
        .unwrap_or(Ok((false, false, index)))
}

async fn index_manager(shared_settings: SharedSettings) {
    loop {
        delay_for(Duration::from_secs(60)).await;
        let (index, toshi_url, max_docs) = {
            let settings = shared_settings.lock().unwrap();
            (
                settings.index,
                settings.toshi_url.clone(),
                settings.max_docs,
            )
        };
        let c = HyperToshi::with_client(&toshi_url, hyper::Client::default());
        debug!("index_manager background checking index {}...", index);
        match check_index(&c, index, max_docs).await {
            Ok((_, false, index)) => loop {
                debug!("Index {} is closed, look for next...", index);
                let next_index = index + 1;
                match check_index(&c, next_index, max_docs).await {
                    Ok((true, true, _)) => {
                        debug!("Using index {} as next active write index", next_index);
                        shared_settings.lock().unwrap().index = next_index;
                        break;
                    }
                    Ok((false, _, _)) => {
                        match c
                            .create_index(make_index_name(next_index), TANTIVY_SCHEMA.clone())
                            .await
                        {
                            Ok(_) => {
                                shared_settings.lock().unwrap().index = next_index;
                                break;
                            }
                            Err(e) => {
                                warn!("Error creating next index in index_manager background task: {:?}", e);
                                break;
                            }
                        }
                    }
                    Ok((true, false, _)) => continue,
                    Err(e) => warn!(
                        "Error looking for next index in index_manager background task: {:?}",
                        e
                    ),
                }
            },
            Ok((_, true, _)) => {}
            Err(e) => warn!("Error in index_manager background task: {:?}", e),
        }
    }
}

async fn find_first_open_index(
    c: &DefaultHyperToshi,
    start: u32,
    max_docs: u32,
) -> Result<u32, Box<dyn Error + Send + Sync>> {
    let mut index = start;
    while let (exists, false, _) = check_index(c, index, max_docs).await? {
        if exists {
            debug!("Does yaffle_{} exist? YES", index);
            index += 1;
        } else {
            debug!("Does yaffle_{} exist? NO", index);
            c.create_index(make_index_name(index), TANTIVY_SCHEMA.clone())
                .await?;
            // We now have a new, open index to use
            break;
        }
    }
    Ok(index)
}

async fn find_index(toshi_url: &str, max_docs: u32) -> Result<u32, Box<dyn Error + Send + Sync>> {
    let client = hyper::Client::default();
    let c = HyperToshi::with_client(toshi_url, client);
    // TODO: avoid iterating when Toshi has a "list all indexes" method
    let mut futures = FuturesOrdered::new();
    for coarse in (0..20).rev() {
        debug!("Pushing check for yaffle_{}", coarse * 10);
        futures.push(check_index(&c, coarse * 10, max_docs));
    }
    'outer: for fine in 1..200 {
        for coarse in (0..20).rev() {
            if let Some(result) = futures.next().await {
                let (exists, _, index) = result?;
                if exists {
                    debug!("Does yaffle_{} exist? YES", index);
                    return Ok(find_first_open_index(&c, index, max_docs).await?);
                } else {
                    debug!("Does yaffle_{} exist? NO", index);
                }
            } else {
                // futures stream is empty, break
                break 'outer;
            }
            if fine < 10 {
                debug!("Pushing check for yaffle_{}", (coarse * 10) + fine);
                futures.push(check_index(&c, (coarse * 10) + fine, max_docs));
            }
        }
    }
    debug!("No yaffle_* indexes at all found, creating new index yaffle_0...");
    c.create_index("yaffle_0", TANTIVY_SCHEMA.clone()).await?;
    Ok(0)
}

pub fn get_our_schema_map() -> &'static HashMap<&'static str, &'static OurSchema<'static>> {
    &OUR_SCHEMA_MAP
}

pub fn get_tantivy_schema() -> &'static Schema {
    &TANTIVY_SCHEMA
}

const COMMIT_EVERY_SECS: u32 = 10;
const BATCH_SIZE: usize = 10;

#[derive(Debug)]
enum Incoming {
    Gelf(gelf::GELFMessage),
    Syslog(syslog::SyslogMessage),
}

async fn do_reverse_dns(doc: &mut JsonMap<'_>, src_ip: IpAddr) {
    if !doc.contains_key("hostname") {
        doc.insert(
            "hostname",
            task::spawn_blocking(move || lookup_addr(&src_ip))
                .await
                .unwrap_or(Ok(src_ip.to_string()))
                .unwrap_or(src_ip.to_string())
                .into(),
        );
    }
}

async fn async_dual_main(
    settings: SharedSettings,
    gelf_rx: mpsc::Receiver<(SocketAddr, gelf::GELFMessage)>,
    syslog_rx: mpsc::Receiver<(SocketAddr, syslog::SyslogMessage)>,
) -> Result<(), Box<dyn Error>> {
    // Merge the two streams of incoming messages
    let gelf_stream = gelf_rx.map(|(src, x)| (src, Incoming::Gelf(x)));
    let syslog_stream = syslog_rx.map(|(src, y)| (src, Incoming::Syslog(y)));
    let merged = gelf_stream.merge(syslog_stream);

    let client = {
        HyperToshi::with_client(
            &settings.lock().unwrap().toshi_url,
            hyper::Client::default(),
        )
    };
    let c = &client;
    let mut last_commit = Instant::now();
    let commit_every = Duration::from_secs(COMMIT_EVERY_SECS.into());

    // Process and submit messages in batch
    let mut chunks = merged.chunks_timeout(BATCH_SIZE, commit_every);
    while let Some(mut chunk) = chunks.next().await {
        debug!("Processing a batch of {} message(s)", chunk.len());
        let index_name = &settings.lock().unwrap().index_name();
        join_all(chunk.drain(..).filter_map(|(src, record)| {
            let mut doc = match record {
                Incoming::Gelf(msg) => extract_gelf_fields(&OUR_SCHEMA, msg),
                Incoming::Syslog(msg) => extract_syslog_fields(&OUR_SCHEMA, msg),
            };

            if doc.len() > 0 {
                let commit = if last_commit.elapsed() >= commit_every {
                    last_commit = Instant::now();
                    true
                } else {
                    false
                };
                let src_ip = src.ip();
                Some(async move {
                    do_reverse_dns(&mut doc, src_ip).await;
                    c.add_document(index_name, Some(IndexOptions { commit }), doc)
                        .await
                })
            } else {
                warn!("Empty document extracted");
                None
            }
        }))
        .await
        .iter()
        .for_each(|result| match result {
            Ok(response) => {
                if !response.status().is_success() {
                    warn!("Error response from add_document: {:?}", response)
                }
            }
            Err(e) => warn!("Failed to insert document: {}", e),
        });
    }

    Ok(())
}

fn extract_syslog_fields<'a>(_schema: &'a [OurSchema], msg: syslog::SyslogMessage) -> JsonMap<'a> {
    let mut map = HashMap::new();
    map.insert("priority", Value::Number(msg.priority.into()));
    map.insert("syslog_facility", Value::String(msg.facility));
    map.insert(
        "source_timestamp",
        Value::Number(
            (msg.source_timestamp.timestamp() * 1_000_000i64
                + msg.source_timestamp.timestamp_subsec_micros() as i64)
                .into(),
        ),
    );
    if let Some(ref hostname) = msg.hostname {
        map.insert("hostname", Value::String(hostname.to_string()));
    }
    if let Some(ref identifier) = msg.identifier {
        map.insert("syslog_identifier", Value::String(identifier.to_string()));
    }
    if let Some(pid) = msg.pid {
        map.insert("syslog_pid", Value::Number(pid.into()));
    }
    map.insert("message", Value::String(msg.message.clone()));
    map.insert("full_message", Value::String(msg.full_message.clone()));
    map
}

fn extract_gelf_fields<'a>(schema: &[OurSchema<'a>], msg: gelf::GELFMessage) -> JsonMap<'a> {
    let gelf::GELFMessage {
        version: _,
        host,
        short_message,
        other,
    } = msg;
    let mut map = HashMap::new();
    map.insert("message", Value::String(short_message));
    map.insert("hostname", Value::String(host));
    for field in schema {
        for gelf_field in field.from_gelf {
            let field_name = match gelf_field {
                Convert::None(f)
                | Convert::FloatSecToUsec(f)
                | Convert::SyslogTimestamp(f)
                | Convert::HexToUint(f) => f,
            };
            if let Some(msg_val) = other.get(*field_name) {
                // Convert type to Tantivy type
                convert(msg_val, gelf_field, &field.kind)
                    .and_then(|converted| Ok(map.insert(field.name, converted)))
                    .unwrap_or_else(|e| {
                        warn!(
                            "Unable to convert GELF field {} containing {} to {}: {}",
                            field_name, msg_val, field.name, e
                        );
                        None
                    });
            }
        }
    }
    map
}

fn convert(
    input: &Value,
    input_conversion: &Convert,
    field_type: &FieldType,
) -> Result<Value, Box<dyn Error>> {
    match (field_type, input) {
        (FieldType::String, Value::String(s)) | (FieldType::Text, Value::String(s)) => {
            Ok(Value::String(s.to_string()))
        }
        (FieldType::String, anything) | (FieldType::Text, anything) => {
            Ok(Value::String(anything.to_string()))
        }
        (FieldType::U64, Value::String(s)) => Ok(Value::Number(
            if let Convert::HexToUint(_) = input_conversion {
                u64::from_str_radix(s, 16)?.into()
            } else {
                s.parse::<u64>()?.into()
            },
        )),
        (FieldType::U64, Value::Number(n)) if n.is_u64() => Ok(input.clone()),
        (FieldType::U64, other) => Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Can't represent {} as u64", other),
        ))),
        (FieldType::I64, Value::String(s)) => Ok(Value::Number(
            if let Convert::HexToUint(_) = input_conversion {
                i64::from_str_radix(s, 16)?.into()
            } else {
                s.parse::<i64>()?.into()
            },
        )),
        (FieldType::I64, Value::Number(n)) if n.is_i64() => Ok(input.clone()),
        (FieldType::I64, other) => Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Can't represent {} as i64", other),
        ))),
        (FieldType::Timestamp, Value::String(s)) => match input_conversion {
            Convert::None(_) => Ok(s.parse::<u64>()?.into()),
            Convert::HexToUint(_) => Ok(u64::from_str_radix(s, 16)?.into()),
            Convert::FloatSecToUsec(_) => {
                let v: f64 = s.parse()?;
                Ok(((v * 1_000_000f64) as u64).into())
            }
            Convert::SyslogTimestamp(_) => Ok(DateTime::parse_from_rfc3339(s)
                .map(|dt| dt.timestamp_nanos() / 1000)
                .or_else(|_e| DateTime::parse_from_rfc2822(s).map(|dt| dt.timestamp_nanos() / 1000))
                .or_else(|_e| {
                    /* TODO: local time? */
                    let now_s = format!("{} {}", Utc::now().format("%Y"), s);
                    NaiveDateTime::parse_from_str(&now_s, "%Y %b %e %T")
                        .map(|naive| naive.timestamp_nanos() / 1000)
                })?
                .into()),
        },
        (FieldType::Timestamp, Value::Number(n)) => match input_conversion {
            Convert::None(_) => Ok(Value::Number(
                input
                    .as_u64()
                    .ok_or(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("Can't represent {} as u64", input),
                    ))?
                    .into(),
            )),
            Convert::FloatSecToUsec(_) => {
                let n_usec = n.as_f64().ok_or(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("Can't represent {} as f64", n),
                ))? * 1_000_000f64;
                Ok(Value::Number((n_usec as u64).into()))
            }
            Convert::SyslogTimestamp(_) => Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Can't convert {} as syslog timestamp", n),
            ))),
            Convert::HexToUint(_) => Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Can't convert {} as hex", n),
            ))),
        },
        (FieldType::Timestamp, other) => Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Can't use {} as timestamp", other),
        ))),
    }
}

#[derive(Debug, Eq, PartialEq)]
enum Convert<'a> {
    FloatSecToUsec(&'a str),
    SyslogTimestamp(&'a str),
    HexToUint(&'a str),
    None(&'a str),
}

#[derive(Debug)]
enum FieldType {
    String,
    Text,
    U64,
    I64,
    Timestamp,
}

pub struct OurSchema<'a> {
    name: &'a str,
    kind: FieldType,
    from_gelf: &'a [Convert<'a>],
}

impl OurSchema<'_> {
    fn get_type(&self) -> &FieldType {
        &self.kind
    }
}

lazy_static! {
    static ref OUR_SCHEMA: &'static [OurSchema<'static>] = &[
        OurSchema {
            name: "message",
            kind: FieldType::Text,
            from_gelf: &[Convert::None("short_message")],
        },
        OurSchema {
            name: "full_message",
            kind: FieldType::Text,
            from_gelf: &[Convert::None("full_message")],
        },
        OurSchema {
            name: "message_id",
            kind: FieldType::String,
            from_gelf: &[Convert::None("_MESSAGE_ID")],
        },
        OurSchema {
            name: "priority",
            kind: FieldType::U64,
            from_gelf: &[Convert::None("level"), Convert::None("_PRIORITY")],
        },
        OurSchema {
            name: "code_file",
            kind: FieldType::Text,
            from_gelf: &[Convert::None("file"), Convert::None("_CODE_FILE")],
        },
        OurSchema {
            name: "code_line",
            kind: FieldType::U64,
            from_gelf: &[Convert::None("line"), Convert::None("_CODE_LINE")],
        },
        OurSchema {
            name: "code_func",
            kind: FieldType::String,
            from_gelf: &[Convert::None("_CODE_FUNC"), Convert::None("_function")],
        },
        OurSchema {
            name: "errno",
            kind: FieldType::I64,
            from_gelf: &[Convert::None("_ERRNO")],
        },
        OurSchema {
            name: "invocation_id",
            kind: FieldType::String,
            from_gelf: &[Convert::None("_INVOCATION_ID")],
        },
        OurSchema {
            name: "user_invocation_id",
            kind: FieldType::String,
            from_gelf: &[Convert::None("_USER_INVOCATION_ID")],
        },
        OurSchema {
            name: "syslog_facility",
            kind: FieldType::String,
            from_gelf: &[Convert::None("facility"), Convert::None("_SYSLOG_FACILITY")],
        },
        OurSchema {
            name: "syslog_identifier",
            kind: FieldType::String,
            from_gelf: &[Convert::None("_SYSLOG_IDENTIFIER")],
        },
        OurSchema {
            name: "syslog_pid",
            kind: FieldType::U64,
            from_gelf: &[Convert::None("_SYSLOG_PID")],
        },
        OurSchema {
            name: "syslog_timestamp",
            kind: FieldType::Timestamp,
            from_gelf: &[Convert::SyslogTimestamp("_SYSLOG_TIMESTAMP")],
        },
        OurSchema {
            name: "pid",
            kind: FieldType::U64,
            from_gelf: &[Convert::None("_PID")],
        },
        OurSchema {
            name: "uid",
            kind: FieldType::U64,
            from_gelf: &[Convert::None("_UID")],
        },
        OurSchema {
            name: "gid",
            kind: FieldType::U64,
            from_gelf: &[Convert::None("_GID")],
        },
        OurSchema {
            name: "comm",
            kind: FieldType::String,
            from_gelf: &[Convert::None("_COMM")],
        },
        OurSchema {
            name: "exe",
            kind: FieldType::String,
            from_gelf: &[Convert::None("_EXE")],
        },
        OurSchema {
            name: "cmdline",
            kind: FieldType::Text,
            from_gelf: &[Convert::None("_CMDLINE")],
        },
        OurSchema {
            name: "cap_effective",
            kind: FieldType::U64,
            from_gelf: &[Convert::HexToUint("_CAP_EFFECTIVE")],
        },
        OurSchema {
            name: "audit_session",
            kind: FieldType::U64,
            from_gelf: &[Convert::None("_AUDIT_SESSION")],
        },
        OurSchema {
            name: "audit_loginuid",
            kind: FieldType::U64,
            from_gelf: &[Convert::None("_AUDIT_LOGINUID")],
        },
        OurSchema {
            name: "systemd_cgroup",
            kind: FieldType::Text,
            from_gelf: &[Convert::None("_SYSTEMD_CGROUP")],
        },
        OurSchema {
            name: "systemd_slice",
            kind: FieldType::Text,
            from_gelf: &[Convert::None("_SYSTEMD_SLICE")],
        },
        OurSchema {
            name: "systemd_unit",
            kind: FieldType::Text,
            from_gelf: &[Convert::None("_SYSTEMD_UNIT")],
        },
        OurSchema {
            name: "systemd_user_unit",
            kind: FieldType::Text,
            from_gelf: &[Convert::None("_SYSTEMD_USER_UNIT")],
        },
        OurSchema {
            name: "systemd_user_slice",
            kind: FieldType::Text,
            from_gelf: &[Convert::None("_SYSTEMD_USER_SLICE")],
        },
        OurSchema {
            name: "systemd_session",
            kind: FieldType::String,
            from_gelf: &[Convert::None("_SYSTEMD_SESSION")],
        },
        OurSchema {
            name: "systemd_owner_uid",
            kind: FieldType::U64,
            from_gelf: &[Convert::None("_SYSTEMD_OWNER_UID")],
        },
        OurSchema {
            name: "selinux_context",
            kind: FieldType::String,
            from_gelf: &[Convert::None("_SELINUX_CONTEXT")],
        },
        OurSchema {
            name: "source_timestamp",
            kind: FieldType::Timestamp,
            from_gelf: &[
                Convert::FloatSecToUsec("timestamp"),
                Convert::None("_SOURCE_REALTIME_TIMESTAMP")
            ],
        },
        OurSchema {
            name: "boot_id",
            kind: FieldType::String,
            from_gelf: &[Convert::None("_BOOT_ID")],
        },
        OurSchema {
            name: "machine_id",
            kind: FieldType::String,
            from_gelf: &[Convert::None("_MACHINE_ID")],
        },
        OurSchema {
            name: "systemd_invocation_id",
            kind: FieldType::String,
            from_gelf: &[Convert::None("_SYSTEMD_INVOCATION_ID")],
        },
        OurSchema {
            name: "hostname",
            kind: FieldType::String,
            from_gelf: &[Convert::None("host"), Convert::None("_HOSTNAME")],
        },
        OurSchema {
            name: "transport",
            kind: FieldType::String,
            from_gelf: &[Convert::None("_TRANSPORT")],
        },
        OurSchema {
            name: "stream_id",
            kind: FieldType::String,
            from_gelf: &[Convert::None("_STREAM_ID")],
        },
        OurSchema {
            name: "line_break",
            kind: FieldType::String,
            from_gelf: &[Convert::None("_LINE_BREAK")],
        },
        OurSchema {
            name: "namespace",
            kind: FieldType::String,
            from_gelf: &[Convert::None("_NAMESPACE")],
        },
        OurSchema {
            name: "kernel_device",
            kind: FieldType::Text,
            from_gelf: &[Convert::None("_KERNEL_DEVICE")],
        },
        OurSchema {
            name: "kernel_subsystem",
            kind: FieldType::String,
            from_gelf: &[Convert::None("_KERNEL_SUBSYSTEM")],
        },
        OurSchema {
            name: "udev_sysname",
            kind: FieldType::Text,
            from_gelf: &[Convert::None("_UDEV_SYSNAME")],
        },
        OurSchema {
            name: "udev_devnode",
            kind: FieldType::Text,
            from_gelf: &[Convert::None("_UDEV_DEVNODE")],
        },
        OurSchema {
            name: "udev_devlink",
            kind: FieldType::Text,
            from_gelf: &[Convert::None("_UDEV_DEVLINK")],
        },
        OurSchema {
            name: "coredump_unit",
            kind: FieldType::Text,
            from_gelf: &[Convert::None("_COREDUMP_UNIT")],
        },
        OurSchema {
            name: "coredump_user_unit",
            kind: FieldType::Text,
            from_gelf: &[Convert::None("_COREDUMP_USER_UNIT")],
        },
        OurSchema {
            name: "object_pid",
            kind: FieldType::U64,
            from_gelf: &[Convert::None("_OBJECT_PID")],
        },
        OurSchema {
            name: "object_uid",
            kind: FieldType::U64,
            from_gelf: &[Convert::None("_OBJECT_UID")],
        },
        OurSchema {
            name: "object_gid",
            kind: FieldType::U64,
            from_gelf: &[Convert::None("_OBJECT_GID")],
        },
        OurSchema {
            name: "object_comm",
            kind: FieldType::String,
            from_gelf: &[Convert::None("_OBJECT_COMM")],
        },
        OurSchema {
            name: "object_exe",
            kind: FieldType::String,
            from_gelf: &[Convert::None("_OBJECT_EXE")],
        },
        OurSchema {
            name: "object_cmdline",
            kind: FieldType::Text,
            from_gelf: &[Convert::None("_OBJECT_CMDLINE")],
        },
        OurSchema {
            name: "object_audit_session",
            kind: FieldType::U64,
            from_gelf: &[Convert::None("_OBJECT_AUDIT_SESSION")],
        },
        OurSchema {
            name: "object_audit_loginuid",
            kind: FieldType::U64,
            from_gelf: &[Convert::None("_OBJECT_AUDIT_LOGINUID")],
        },
        OurSchema {
            name: "object_systemd_cgroup",
            kind: FieldType::Text,
            from_gelf: &[Convert::None("_OBJECT_SYSTEMD_CGROUP")],
        },
        OurSchema {
            name: "object_systemd_session",
            kind: FieldType::String,
            from_gelf: &[Convert::None("_OBJECT_SYSTEMD_SESSION")],
        },
        OurSchema {
            name: "object_systemd_owner_uid",
            kind: FieldType::U64,
            from_gelf: &[Convert::None("_OBJECT_SYSTEMD_OWNER_UID")],
        },
        OurSchema {
            name: "object_systemd_unit",
            kind: FieldType::Text,
            from_gelf: &[Convert::None("_OBJECT_SYSTEMD_UNIT")],
        },
        OurSchema {
            name: "object_systemd_user_unit",
            kind: FieldType::Text,
            from_gelf: &[Convert::None("_OBJECT_SYSTEMD_USER_UNIT")],
        },
        OurSchema {
            name: "recv_rt_timestamp",
            kind: FieldType::Timestamp,
            from_gelf: &[Convert::None("___REALTIME_TIMESTAMP")],
        },
        OurSchema {
            name: "recv_mt_timestamp",
            kind: FieldType::Timestamp,
            from_gelf: &[Convert::None("___MONOTONIC_TIMESTAMP")],
        },
    ];
    static ref TANTIVY_SCHEMA: Schema = {
        let mut schema_builder = SchemaBuilder::default();
        for field in OUR_SCHEMA.iter() {
            match field.kind {
                FieldType::String => schema_builder.add_text_field(field.name, STORED | STRING),
                FieldType::Text => schema_builder.add_text_field(field.name, STORED | TEXT),
                FieldType::U64 => schema_builder.add_u64_field(field.name, STORED | INDEXED),
                FieldType::I64 => schema_builder.add_i64_field(field.name, STORED | INDEXED),
                FieldType::Timestamp => {
                    schema_builder.add_u64_field(field.name, INDEXED | STORED | FAST)
                }
            };
        }
        schema_builder.build()
    };
    static ref OUR_SCHEMA_MAP: HashMap<&'static str, &'static OurSchema<'static>> = {
        let mut map = HashMap::new();
        for field in OUR_SCHEMA.iter() {
            map.insert(field.name, field);
        }
        map
    };
}
