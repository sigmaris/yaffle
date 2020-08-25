use std::collections::HashMap;
use std::error::Error;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::stream::StreamExt;

use futures::future::join_all;
use futures::stream::FuturesOrdered;
use futures_batch::ChunksTimeoutStreamExt;
use lazy_static::lazy_static;
use listenfd::ListenFd;
use log::{debug, warn};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tantivy::schema::Schema;
use tantivy::space_usage::SearcherSpaceUsage;
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::delay_for;
use toshi::{Client, HyperToshi, IndexOptions};

mod gelf;
mod query;
mod schema;
mod syslog;
mod websrv;

use schema::{Document, YaffleSchema};

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

const COMMIT_EVERY_SECS: u32 = 10;
const BATCH_SIZE: usize = 10;

#[derive(Debug)]
enum Incoming {
    Gelf(gelf::GELFMessage),
    Syslog(syslog::SyslogMessage),
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
        let mut batch_results = join_all(chunk.drain(..).filter_map(|(src, record)| {
            let doc_result = match record {
                Incoming::Gelf(msg) => Document::from_gelf(&msg),
                Incoming::Syslog(msg) => Document::from_syslog(&msg),
            };

            match doc_result {
                Ok(mut doc) if doc.is_valid() => {
                    let commit = if last_commit.elapsed() >= commit_every {
                        last_commit = Instant::now();
                        true
                    } else {
                        false
                    };
                    let src_ip = src.ip();
                    Some(async move {
                        doc.do_reverse_dns(src_ip).await;
                        c.add_document(index_name, Some(IndexOptions { commit }), doc)
                            .await
                    })
                }
                Ok(invalid_doc) => {
                    warn!("Invalid document extracted: {:?}", invalid_doc);
                    None
                }
                Err(e) => {
                    warn!("Document parsing error: {}", e);
                    None
                }
            }
        }))
        .await;
        for result in batch_results.drain(..) {
            match result {
                Ok(response) => {
                    if !response.status().is_success() {
                        let response_msg =
                            format!("Error response from add_document: {:?}", response);
                        match response
                            .into_body()
                            .collect::<Result<bytes::Bytes, _>>()
                            .await
                        {
                            Ok(bytes) => warn!(
                                "{}: {}",
                                response_msg,
                                String::from_utf8_lossy(bytes.as_ref())
                            ),
                            Err(e) => warn!("{}: body read error: {}", response_msg, e),
                        }
                    }
                }
                Err(e) => warn!("Failed to insert document: {}", e),
            }
        }
    }

    Ok(())
}

lazy_static! {
    static ref TANTIVY_SCHEMA: Schema = Document::tantivy_schema();
}
