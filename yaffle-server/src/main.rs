mod gelf;
mod quickwit;
mod schema;
mod syslog;
mod websrv;

use crate::quickwit::{DocumentMapping, RetentionSettings, SearchSettings};
use crate::schema::{Document, YaffleSchema};
use clap::Parser;
use hyper::client::HttpConnector;
use hyper::http::uri::InvalidUri;
use hyper::{Body, Client, Request, StatusCode, Uri};
use lazy_static::lazy_static;
use listenfd::ListenFd;
use log::{debug, error, warn};
use nom::AsBytes;
use std::io::Write;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tantivy::schema::Schema;
use thiserror::Error;
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;
use url::ParseError;

type SharedSettings = Arc<Mutex<Settings>>;

#[derive(Error, Debug)]
pub enum YaffleError {
    #[error("I/O Error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Invalid Quickwit URI: {0}")]
    UriBuild(#[from] InvalidUri),
    #[error("Invalid Quickwit URI: {0}")]
    UriParse(#[from] ParseError),
    #[error("Quickwit Client Error: {0}")]
    HyperClient(#[from] hyper::Error),
    #[error("Create Index Error ({0}): {1}")]
    CreateIndex(StatusCode, String),
    #[error("HTTP Error: {0}")]
    Http(#[from] hyper::http::Error),
    #[error("JSON Serialization Error: {0}")]
    JsonSerialize(#[from] serde_json::Error),
}

#[derive(Parser, Debug)]
struct Args {
    #[arg(
        short = 'q',
        default_value = "http://localhost:7280",
        env = "QUICKWIT_URL"
    )]
    quickwit_url: String,
    #[arg(short = 'i', default_value = "yaffle_logs", env = "QUICKWIT_INDEX")]
    quickwit_index: String,
}

pub struct Settings {
    pub quickwit_url: String,
    pub quickwit_index: String,
}

fn main() -> Result<(), YaffleError> {
    env_logger::init();
    let Args {
        mut quickwit_url,
        quickwit_index,
    } = Args::parse();
    let rt = tokio::runtime::Runtime::new().unwrap();

    if quickwit_url.ends_with("/") {
        quickwit_url.pop();
    }

    let client = hyper::Client::builder()
        .pool_idle_timeout(Duration::from_secs(30))
        .build_http::<hyper::Body>();
    rt.block_on(get_or_create_index(&client, &quickwit_url, &quickwit_index))?;

    // TODO: no need for Arc/Mutex now these are immutable?
    let settings = Arc::new(Mutex::new(Settings {
        quickwit_url,
        quickwit_index,
    }));

    let mut listenfd = ListenFd::from_env();

    // Listen for GELF messages
    let (gelf_tx, gelf_rx) = mpsc::channel(10);
    let gelf_sock = if let Some(std_sock) = listenfd.take_udp_socket(0).ok().flatten() {
        debug!("Using passed GELF UDP socket {:?}", std_sock);
        std_sock.set_nonblocking(true)?;
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
        std_sock.set_nonblocking(true)?;
        rt.block_on(async { UdpSocket::from_std(std_sock) })?
    } else {
        debug!("Binding to [::]:10514 for Syslog UDP");
        rt.block_on(UdpSocket::bind("[::]:10514"))?
    };

    rt.spawn(async { syslog::run_recv_loop(syslog_sock, syslog_tx).await.unwrap() });

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    // Create HTTP listener and then leak it for the remainder of the program's lifetime
    let http_listener = if let Some(std_sock) = listenfd.take_tcp_listener(2).ok().flatten() {
        debug!("Using passed HTTP socket {:?}", std_sock);
        std_sock.set_nonblocking(true)?;
        rt.block_on(async { TcpListener::from_std(std_sock) })?
    } else {
        debug!("Binding to [::]:8088 for HTTP");
        rt.block_on(TcpListener::bind("[::]:8088"))?
    };
    let http_stream = tokio_stream::wrappers::TcpListenerStream::new(http_listener);
    rt.spawn(websrv::run_http_server(
        settings.clone(),
        http_stream,
        shutdown_rx,
    ));

    rt.block_on(async {
        ingest_loop(&client, settings, gelf_rx, syslog_rx)
            .await
            .unwrap()
    });
    shutdown_tx.send(()).ok();

    Ok(())
}

async fn get_or_create_index(
    client: &Client<HttpConnector>,
    quickwit_url: &str,
    quickwit_index: &str,
) -> Result<(), YaffleError> {
    let describe_uri = [&quickwit_url, "api/v1/indexes", quickwit_index, "describe"].join("/");
    let response = client.get(Uri::from_str(&describe_uri)?).await?;
    if response.status() == StatusCode::NOT_FOUND {
        debug!("No {} index found, creating new index...", quickwit_index);
        let create_uri = [&quickwit_url, "api/v1/indexes"].join("/");
        let request_body = quickwit::IndexCreateRequest {
            doc_mapping: DocumentMapping {
                field_mappings: Document::quickwit_mapping(),
                mode: None,
                tag_fields: Vec::default(),
                store_source: false,
                timestamp_field: "source_timestamp".to_string(),
            },
            index_id: quickwit_index.to_string(),
            retention: RetentionSettings {
                period: "90 days".to_string(),
                schedule: "daily".to_string(),
            },
            search_settings: SearchSettings {
                default_search_fields: vec!["message".to_string(), "full_message".to_string()],
            },
            version: "0.5".to_string(),
        };
        let mut response = client
            .request(
                Request::builder()
                    .uri(Uri::from_str(&create_uri)?)
                    .method("POST")
                    .header("Content-Type", "application/json")
                    .body(Body::from(serde_json::to_string(&request_body)?))?,
            )
            .await?;
        if !response.status().is_success() {
            return Err(YaffleError::CreateIndex(
                response.status(),
                String::from_utf8_lossy(
                    hyper::body::to_bytes(response.body_mut()).await?.as_bytes(),
                )
                .to_string(),
            ));
        }
    } else {
        debug!("Found existing index {}", quickwit_index);
    }
    Ok(())
}

const COMMIT_EVERY_SECS: u64 = 10;
const BATCH_SIZE: usize = 10;

#[derive(Debug)]
enum Incoming {
    Gelf(gelf::GELFMessage),
    Syslog(syslog::SyslogMessage),
}

async fn ingest_loop(
    client: &Client<HttpConnector>,
    settings: SharedSettings,
    gelf_rx: mpsc::Receiver<(SocketAddr, gelf::GELFMessage)>,
    syslog_rx: mpsc::Receiver<(SocketAddr, syslog::SyslogMessage)>,
) -> Result<(), YaffleError> {
    // Merge the two streams of incoming messages
    let gelf_stream = ReceiverStream::new(gelf_rx).map(|(src, x)| (src, Incoming::Gelf(x)));
    let syslog_stream = ReceiverStream::new(syslog_rx).map(|(src, y)| (src, Incoming::Syslog(y)));
    let merged = gelf_stream.merge(syslog_stream);

    // Process and submit messages in batch
    let mut chunks =
        Box::pin(merged.chunks_timeout(BATCH_SIZE, Duration::from_secs(COMMIT_EVERY_SECS)));
    while let Some(ref chunk) = chunks.next().await {
        debug!("Processing a batch of {} message(s)", chunk.len());
        let mut buffer = Vec::new();
        for (src, record) in chunk {
            let doc_result = match record {
                Incoming::Gelf(msg) => Document::from_gelf(&msg),
                Incoming::Syslog(msg) => Document::from_syslog(&msg),
            };

            match doc_result {
                Ok(mut doc) if doc.is_valid() => {
                    let src_ip = src.ip();
                    doc.do_reverse_dns(src_ip).await;
                    serde_json::to_writer(&mut buffer, &doc).unwrap_or_else(|serde_err| {
                        warn!("Unserializable document ({}): {:?}", serde_err, doc);
                    });
                    write!(buffer, "\n").ok();
                }
                Ok(invalid_doc) => {
                    warn!("Invalid document extracted: {:?}", invalid_doc);
                }
                Err(e) => {
                    warn!("Document parsing error: {}", e);
                }
            }
        }
        debug!("Average size per document: {}", buffer.len() / chunk.len());

        let ingest_uri = {
            let locked = settings.lock().unwrap();
            [
                &locked.quickwit_url,
                "api/v1",
                &locked.quickwit_index,
                "ingest",
            ]
            .join("/")
        };
        match client
            .request(
                Request::builder()
                    .uri(Uri::from_str(&ingest_uri)?)
                    .method("POST")
                    .body(Body::from(buffer))?,
            )
            .await
        {
            Ok(mut response) => {
                if !response.status().is_success() {
                    error!(
                        "Error from Quickwit in ingest ({}): {}",
                        response.status(),
                        String::from_utf8_lossy(
                            hyper::body::to_bytes(response.body_mut()).await?.as_bytes(),
                        )
                    );
                }
            }
            Err(http_err) => error!("Error submitting batch to Quickwit: {}", http_err),
        }
    }

    Ok(())
}

lazy_static! {
    static ref TANTIVY_SCHEMA: Schema = Document::tantivy_schema();
}
