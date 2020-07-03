use std::collections::HashMap;
use std::error::Error;

use lazy_static::lazy_static;
use listenfd::ListenFd;
use log::{debug, warn};
use serde_json::{Number, Value};
use tantivy::schema::{Schema, SchemaBuilder, INDEXED, STORED, STRING, TEXT};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use toshi::{Client, HyperToshi, IndexOptions};

mod gelf;
mod websrv;
type JsonMap<'a> = HashMap<&'a str, Value>;

fn main() -> Result<(), Box<dyn Error>> {
    env_logger::init();
    let mut rt = tokio::runtime::Runtime::new().unwrap();

    let mut listenfd = ListenFd::from_env();
    let (gelf_tx, gelf_rx) = mpsc::channel(10);
    let gelf_sock = if let Some(std_sock) = listenfd.take_udp_socket(0).ok().flatten() {
        debug!("Using passed GELF UDP socket {:?}", std_sock);
        rt.block_on(async { UdpSocket::from_std(std_sock) })?
    } else {
        debug!("Binding to [::]:12201 for GELF UDP");
        rt.block_on(UdpSocket::bind("[::]:12201"))?
    };

    rt.spawn(async { gelf::run_recv_loop(gelf_sock, gelf_tx).await.unwrap() });

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    // Create HTTP listener and then leak it for the remainder of the program's lifetime
    let http_listener: &'static mut TcpListener = Box::leak(Box::new(
        if let Some(std_sock) = listenfd.take_tcp_listener(1).ok().flatten() {
            debug!("Using passed HTTP socket {:?}", std_sock);
            rt.block_on(async { TcpListener::from_std(std_sock) })?
        } else {
            debug!("Binding to [::]:8088 for HTTP");
            rt.block_on(TcpListener::bind("[::]:8088"))?
        },
    ));

    rt.spawn(websrv::run_http_server(http_listener, shutdown_rx));
    rt.block_on(async { async_main(gelf_rx).await.unwrap() });
    shutdown_tx.send(()).ok();

    Ok(())
}

pub fn get_our_schema_map() -> &'static HashMap<&'static str, &'static OurSchema<'static>> {
    &OUR_SCHEMA_MAP
}

async fn async_main(mut gelf_rx: mpsc::Receiver<gelf::GELFMessage>) -> Result<(), Box<dyn Error>> {
    // Check index exists
    // TODO: check schema?
    let client = hyper::Client::default();
    let c = HyperToshi::with_client("http://localhost:8080", client);
    let body =
        hyper::body::to_bytes(c.index_summary("rustylog_0", false).await?.into_body()).await?;

    // Hack since Toshi returns 200 on nonexistent index
    let summary: JsonMap = serde_json::from_slice(&body)?;
    if summary.contains_key("summaries") {
        debug!("Index already exists");
    } else if summary.contains_key("message") {
        debug!(
            "Index doesn't exist ({:?}), creating new index...",
            summary.get("message")
        );
        c.create_index("rustylog_0", TANTIVY_SCHEMA.clone()).await?;
    } else {
        return Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("Unexpected response from index_summary: {:?}", summary),
        )));
    }

    while let Some(msg) = gelf_rx.recv().await {
        let doc = extract_gelf_fields(&OUR_SCHEMA, &msg);
        if doc.len() > 0 {
            if let Err(e) = c
                .add_document("rustylog_0", Some(IndexOptions { commit: true }), doc)
                .await
            {
                warn!("Failed to insert document: {}", e);
            }
        } else {
            warn!("Empty document extracted from {:?}", msg);
        }
    }

    Ok(())
}

fn extract_gelf_fields<'a>(schema: &[OurSchema<'a>], msg: &'a gelf::GELFMessage) -> JsonMap<'a> {
    let mut map = HashMap::new();
    for field in schema {
        for gelf_field in field.from_gelf {
            if gelf_field == &Convert::None("short_message") {
                map.insert(field.name, Value::String(msg.short_message.clone()));
                break;
            } else if gelf_field == &Convert::None("host") {
                map.insert(field.name, Value::String(msg.host.clone()));
                break;
            } else {
                let field_name = match gelf_field {
                    Convert::None(f) | Convert::USecToFloatSec(f) => f,
                };
                if let Some(msg_val) = msg.other.get(*field_name) {
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
        (FieldType::U64, Value::String(s)) => Ok(Value::Number(s.parse::<u64>()?.into())),
        (FieldType::U64, Value::Number(n)) if n.is_u64() => Ok(input.clone()),
        (FieldType::U64, other) => Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Can't represent {} as u64", other),
        ))),
        (FieldType::I64, Value::String(s)) => Ok(Value::Number(s.parse::<i64>()?.into())),
        (FieldType::I64, Value::Number(n)) if n.is_i64() => Ok(input.clone()),
        (FieldType::I64, other) => Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Can't represent {} as i64", other),
        ))),
        (FieldType::Timestamp, Value::String(s)) => {
            let mut v: f64 = s.parse()?;
            if let Convert::USecToFloatSec(_) = input_conversion {
                v /= 1000000f64;
            }
            Ok(Number::from_f64(v)
                .ok_or(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("Can't put {} into JSON Number", v),
                ))
                .map(|n| Value::Number(n))?)
        }
        (FieldType::Timestamp, Value::Number(n)) => match input_conversion {
            Convert::None(_) => Ok(input.clone()),
            Convert::USecToFloatSec(_) => {
                let n_sec = n.as_f64().ok_or(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("Can't represent {} as f64", n),
                ))? / 1000000f64;
                Ok(Number::from_f64(n_sec)
                    .ok_or(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("Can't put {} into JSON Number", n_sec),
                    ))
                    .map(|n| Value::Number(n))?)
            }
        },
        (FieldType::Timestamp, other) => Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Can't use {} as timestamp", other),
        ))),
    }
}

#[derive(Debug, Eq, PartialEq)]
enum Convert<'a> {
    USecToFloatSec(&'a str),
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
            kind: FieldType::U64,
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
            from_gelf: &[Convert::None("_SYSLOG_TIMESTAMP")],
        },
        OurSchema {
            name: "pid",
            kind: FieldType::U64,
            from_gelf: &[Convert::None("__PID")],
        },
        OurSchema {
            name: "uid",
            kind: FieldType::U64,
            from_gelf: &[Convert::None("__UID")],
        },
        OurSchema {
            name: "gid",
            kind: FieldType::U64,
            from_gelf: &[Convert::None("__GID")],
        },
        OurSchema {
            name: "comm",
            kind: FieldType::String,
            from_gelf: &[Convert::None("__COMM")],
        },
        OurSchema {
            name: "exe",
            kind: FieldType::String,
            from_gelf: &[Convert::None("__EXE")],
        },
        OurSchema {
            name: "cmdline",
            kind: FieldType::Text,
            from_gelf: &[Convert::None("__CMDLINE")],
        },
        OurSchema {
            name: "cap_effective",
            kind: FieldType::U64,
            from_gelf: &[Convert::None("__CAP_EFFECTIVE")],
        },
        OurSchema {
            name: "audit_session",
            kind: FieldType::U64,
            from_gelf: &[Convert::None("__AUDIT_SESSION")],
        },
        OurSchema {
            name: "audit_loginuid",
            kind: FieldType::U64,
            from_gelf: &[Convert::None("__AUDIT_LOGINUID")],
        },
        OurSchema {
            name: "systemd_cgroup",
            kind: FieldType::Text,
            from_gelf: &[Convert::None("__SYSTEMD_CGROUP")],
        },
        OurSchema {
            name: "systemd_slice",
            kind: FieldType::Text,
            from_gelf: &[Convert::None("__SYSTEMD_SLICE")],
        },
        OurSchema {
            name: "systemd_unit",
            kind: FieldType::Text,
            from_gelf: &[Convert::None("__SYSTEMD_UNIT")],
        },
        OurSchema {
            name: "systemd_user_unit",
            kind: FieldType::Text,
            from_gelf: &[Convert::None("__SYSTEMD_USER_UNIT")],
        },
        OurSchema {
            name: "systemd_user_slice",
            kind: FieldType::Text,
            from_gelf: &[Convert::None("__SYSTEMD_USER_SLICE")],
        },
        OurSchema {
            name: "systemd_session",
            kind: FieldType::String,
            from_gelf: &[Convert::None("__SYSTEMD_SESSION")],
        },
        OurSchema {
            name: "systemd_owner_uid",
            kind: FieldType::U64,
            from_gelf: &[Convert::None("__SYSTEMD_OWNER_UID")],
        },
        OurSchema {
            name: "selinux_context",
            kind: FieldType::String,
            from_gelf: &[Convert::None("__SELINUX_CONTEXT")],
        },
        OurSchema {
            name: "source_timestamp",
            kind: FieldType::Timestamp,
            from_gelf: &[
                Convert::None("timestamp"),
                Convert::USecToFloatSec("_SOURCE_REALTIME_TIMESTAMP")
            ],
        },
        OurSchema {
            name: "boot_id",
            kind: FieldType::String,
            from_gelf: &[Convert::None("__BOOT_ID")],
        },
        OurSchema {
            name: "machine_id",
            kind: FieldType::String,
            from_gelf: &[Convert::None("__MACHINE_ID")],
        },
        OurSchema {
            name: "systemd_invocation_id",
            kind: FieldType::String,
            from_gelf: &[Convert::None("__SYSTEMD_INVOCATION_ID")],
        },
        OurSchema {
            name: "hostname",
            kind: FieldType::String,
            from_gelf: &[Convert::None("host"), Convert::None("__HOSTNAME")],
        },
        OurSchema {
            name: "transport",
            kind: FieldType::String,
            from_gelf: &[Convert::None("__TRANSPORT")],
        },
        OurSchema {
            name: "stream_id",
            kind: FieldType::String,
            from_gelf: &[Convert::None("__STREAM_ID")],
        },
        OurSchema {
            name: "line_break",
            kind: FieldType::String,
            from_gelf: &[Convert::None("__LINE_BREAK")],
        },
        OurSchema {
            name: "namespace",
            kind: FieldType::String,
            from_gelf: &[Convert::None("__NAMESPACE")],
        },
        OurSchema {
            name: "kernel_device",
            kind: FieldType::Text,
            from_gelf: &[Convert::None("__KERNEL_DEVICE")],
        },
        OurSchema {
            name: "kernel_subsystem",
            kind: FieldType::String,
            from_gelf: &[Convert::None("__KERNEL_SUBSYSTEM")],
        },
        OurSchema {
            name: "udev_sysname",
            kind: FieldType::Text,
            from_gelf: &[Convert::None("__UDEV_SYSNAME")],
        },
        OurSchema {
            name: "udev_devnode",
            kind: FieldType::Text,
            from_gelf: &[Convert::None("__UDEV_DEVNODE")],
        },
        OurSchema {
            name: "udev_devlink",
            kind: FieldType::Text,
            from_gelf: &[Convert::None("__UDEV_DEVLINK")],
        },
        OurSchema {
            name: "coredump_unit",
            kind: FieldType::Text,
            from_gelf: &[Convert::None("__COREDUMP_UNIT")],
        },
        OurSchema {
            name: "coredump_user_unit",
            kind: FieldType::Text,
            from_gelf: &[Convert::None("__COREDUMP_USER_UNIT")],
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
            from_gelf: &[Convert::USecToFloatSec("___REALTIME_TIMESTAMP")],
        },
        OurSchema {
            name: "recv_mt_timestamp",
            kind: FieldType::Timestamp,
            from_gelf: &[Convert::USecToFloatSec("___MONOTONIC_TIMESTAMP")],
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
                FieldType::Timestamp => schema_builder.add_f64_field(field.name, INDEXED | STORED),
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
