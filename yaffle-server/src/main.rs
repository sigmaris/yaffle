mod fileserv;
mod gelf;
mod quickwit;
mod schema;
mod syslog;

use crate::fileserv::file_and_error_handler;
use crate::schema::YaffleSchema;
use app::settings::{Settings, SharedSettings};
use app::*;
use async_trait::async_trait;
use axum::{
    body::Body,
    extract::{Extension, Path, RawQuery, State},
    http::{HeaderMap, Request},
    response::IntoResponse,
    routing::post,
    Router,
};
use clap::Parser;
use hyper::client::HttpConnector;
use hyper::http::uri::InvalidUri;
use hyper::{Client, StatusCode, Uri};
use lazy_static::lazy_static;
use leptos::*;
use leptos_axum::{generate_route_list, LeptosRoutes};
use listenfd::ListenFd;
use log::{debug, error, warn};
use quickwit::SearchResults;
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::net::SocketAddr;
use std::net::TcpListener as StdTcpListener;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tantivy::schema::Schema;
use thiserror::Error;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;
use url::{ParseError, Url};

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

lazy_static! {
    static ref TANTIVY_SCHEMA: Schema = schema::Document::tantivy_schema();
}

pub(crate) async fn get_or_create_index(
    client: &Client<HttpConnector>,
    quickwit_url: &str,
    quickwit_index: &str,
) -> Result<(), YaffleError> {
    let describe_uri = [quickwit_url, "api/v1/indexes", quickwit_index, "describe"].join("/");
    let response = client.get(Uri::from_str(&describe_uri)?).await?;
    if response.status() == StatusCode::NOT_FOUND {
        debug!("No {} index found, creating new index...", quickwit_index);
        let create_uri = [quickwit_url, "api/v1/indexes"].join("/");
        let request_body = quickwit::IndexCreateRequest {
            doc_mapping: quickwit::DocumentMapping {
                field_mappings: schema::Document::quickwit_mapping(),
                mode: None,
                tag_fields: Vec::default(),
                store_source: false,
                timestamp_field: "source_timestamp".to_string(),
            },
            index_id: quickwit_index.to_string(),
            retention: quickwit::RetentionSettings {
                period: "90 days".to_string(),
                schedule: "daily".to_string(),
            },
            search_settings: quickwit::SearchSettings {
                default_search_fields: vec!["message".to_string(), "full_message".to_string()],
            },
            version: "0.5".to_string(),
        };
        let mut response = client
            .request(
                hyper::Request::builder()
                    .uri(Uri::from_str(&create_uri)?)
                    .method("POST")
                    .header("Content-Type", "application/json")
                    .body(hyper::Body::from(serde_json::to_string(&request_body)?))?,
            )
            .await?;
        if !response.status().is_success() {
            return Err(YaffleError::CreateIndex(
                response.status(),
                String::from_utf8_lossy(hyper::body::to_bytes(response.body_mut()).await?.as_ref())
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

pub(crate) async fn ingest_loop(
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
            let mut doc = {
                let decode_result = match record {
                    Incoming::Gelf(msg) => schema::Document::from_gelf(msg),
                    Incoming::Syslog(msg) => schema::Document::from_syslog(msg),
                };
                match decode_result {
                    Ok(valid_doc) if valid_doc.is_valid() => valid_doc,
                    Ok(invalid_doc) => {
                        warn!("Invalid document extracted: {:?}", invalid_doc);
                        continue;
                    }
                    Err(e) => {
                        warn!("Document parsing error: {}", e);
                        continue;
                    }
                }
            };

            let src_ip = src.ip();
            doc.do_reverse_dns(src_ip).await;
            serde_json::to_writer(&mut buffer, &doc).unwrap_or_else(|serde_err| {
                warn!("Unserializable document ({}): {:?}", serde_err, doc);
            });
            writeln!(buffer).ok();
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
                            hyper::body::to_bytes(response.body_mut()).await?.as_ref(),
                        )
                    );
                }
            }
            Err(http_err) => error!("Error submitting batch to Quickwit: {}", http_err),
        }
    }

    Ok(())
}

pub(crate) async fn run_server(
    settings: SharedSettings,
    gelf_socket: UdpSocket,
    syslog_socket: UdpSocket,
) -> Result<(), YaffleError> {
    let client = hyper::Client::builder()
        .pool_idle_timeout(Duration::from_secs(30))
        .build_http::<hyper::Body>();

    let mut connect_wait = 1;
    loop {
        let (url, index) = {
            let locked = settings.lock().unwrap();
            (locked.quickwit_url.clone(), locked.quickwit_index.clone())
        };

        match get_or_create_index(&client, &url, &index).await {
            Ok(_) => break,
            Err(err) => {
                warn!("Quickwit connect/init error: {}", err);
                tokio::time::sleep(Duration::from_secs(connect_wait)).await;
                connect_wait = (connect_wait * 2).min(10);
            }
        }
    }

    let (gelf_tx, gelf_rx) = mpsc::channel(10);
    let (syslog_tx, syslog_rx) = mpsc::channel(10);

    tokio::spawn(gelf::run_recv_loop(gelf_socket, gelf_tx));
    tokio::spawn(syslog::run_recv_loop(syslog_socket, syslog_tx));
    ingest_loop(&client, settings, gelf_rx, syslog_rx).await
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

async fn handle_server_fns_wrapper(
    State(server_api): State<SharedServerAPI>,
    path: Path<String>,
    headers: HeaderMap,
    raw_query: RawQuery,
    req: Request<Body>,
) -> impl IntoResponse {
    leptos_axum::handle_server_fns_with_context(
        path,
        headers,
        raw_query,
        move |cx| {
            provide_context(cx, server_api.clone());
        },
        req,
    )
    .await
}

#[tokio::main]
async fn main() -> Result<(), YaffleError> {
    simple_logger::init_with_level(log::Level::Debug).expect("couldn't initialize logging");

    let Args {
        mut quickwit_url,
        quickwit_index,
    } = Args::parse();

    if quickwit_url.ends_with('/') {
        quickwit_url.pop();
    }

    let settings: SharedSettings = Arc::new(Mutex::new(Settings {
        quickwit_url,
        quickwit_index,
    }));

    register_server_functions();

    // Setting get_configuration(None) means we'll be using cargo-leptos's env values
    // For deployment these variables are:
    // <https://github.com/leptos-rs/start-axum#executing-a-server-on-a-remote-machine-without-the-toolchain>
    // Alternately a file can be specified such as Some("Cargo.toml")
    // The file would need to be included with the executable when moved to deployment
    let conf = get_configuration(None).await.unwrap();
    let leptos_options = conf.leptos_options;
    let addr = leptos_options.site_addr;
    let routes = generate_route_list(|cx| view! { cx, <App/> }).await;

    // build our application with a route
    let shared_server = Arc::new(ServerAPIImpl {
        settings: settings.clone(),
        client: hyper::Client::builder()
            .pool_idle_timeout(Duration::from_secs(30))
            .build_http::<hyper::Body>(),
    });
    let app = Router::new()
        .route(
            "/api/*fn_name",
            post(handle_server_fns_wrapper).with_state(shared_server.clone()),
        )
        .leptos_routes(
            leptos_options.clone(),
            routes,
            move |cx| view! { cx, <App server_api=shared_server.clone() /> },
        )
        .fallback(file_and_error_handler)
        .layer(Extension(Arc::new(leptos_options)));

    let mut listenfd = ListenFd::from_env();

    // Set up socket to listen for GELF messages
    let gelf_sock = if let Some(std_sock) = listenfd.take_udp_socket(0).ok().flatten() {
        debug!("Using passed GELF UDP socket {:?}", std_sock);
        std_sock.set_nonblocking(true)?;
        UdpSocket::from_std(std_sock)?
    } else {
        debug!("Binding to [::]:12201 for GELF UDP");
        UdpSocket::bind("[::]:12201").await?
    };

    // Listen for Syslog messages
    let syslog_sock = if let Some(std_sock) = listenfd.take_udp_socket(1).ok().flatten() {
        debug!("Using passed Syslog UDP socket {:?}", std_sock);
        std_sock.set_nonblocking(true)?;
        UdpSocket::from_std(std_sock)?
    } else {
        debug!("Binding to [::]:10514 for Syslog UDP");
        UdpSocket::bind("[::]:10514").await?
    };

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    // Create HTTP listener and then leak it for the remainder of the program's lifetime
    let http_listener = if let Some(std_sock) = listenfd.take_tcp_listener(2).ok().flatten() {
        debug!("Using passed HTTP socket {:?}", std_sock);
        std_sock
    } else {
        debug!("Binding to {} for HTTP", addr);
        StdTcpListener::bind(addr)?
    };
    // run our app with hyper
    // `axum::Server` is a re-export of `hyper::Server`
    let server = axum::Server::from_tcp(http_listener)
        .expect("Creating HTTP server on TCP port")
        .serve(app.into_make_service());

    tokio::spawn(async {
        if let Err(e) = run_server(settings, gelf_sock, syslog_sock).await {
            error!("GELF/Syslog receiver exited with error: {}", e);
        }
        shutdown_tx.send(()).ok();
    });

    server
        .with_graceful_shutdown(async {
            shutdown_rx.await.ok();
        })
        .await?;

    Ok(())
}

fn build_search_uri(settings: &SharedSettings, sp: &SearchOptions) -> Result<Uri, YaffleError> {
    let locked = settings.lock().unwrap();
    let mut search_url = Url::parse(&locked.quickwit_url)?;
    search_url
        .path_segments_mut()
        .unwrap()
        .extend(["api", "v1", &locked.quickwit_index, "search"]);
    search_url
        .query_pairs_mut()
        .append_pair("query", if sp.query.is_empty() { "*" } else { &sp.query })
        .append_pair("sort_by_field", "-source_timestamp");
    debug!("Search URI: {}", search_url.as_str());
    Ok(Uri::from_str(search_url.as_str())?)
}

struct ServerAPIImpl {
    settings: SharedSettings,
    client: Client<HttpConnector>,
}

#[async_trait]
impl ServerAPI for ServerAPIImpl {
    async fn search(
        &self,
        options: &SearchOptions,
    ) -> Result<(Vec<String>, Vec<Vec<Option<String>>>), String> {
        let search_uri = build_search_uri(&self.settings, options).map_err(|e| e.to_string())?;

        let mut response = self
            .client
            .get(search_uri)
            .await
            .map_err(|e| e.to_string())?;
        let sr: SearchResults = hyper::body::to_bytes(response.body_mut())
            .await
            .map_err(|e| e.to_string())
            .and_then(|body_bytes| {
                serde_json::from_slice(&body_bytes).map_err(|e| e.to_string())
            })?;
        let mut keys: HashSet<String> = HashSet::new();
        let hashmaps: Vec<HashMap<&str, Cow<'_, str>>> = sr
            .hits
            .iter()
            .map(|doc| {
                let doc_hashmap: HashMap<&str, Cow<'_, str>> = doc.into();
                keys.extend(doc_hashmap.keys().map(|key| key.to_string()));
                doc_hashmap
            })
            .collect();
        let mut keys = Vec::from_iter(keys);
        keys.sort_by(field_custom_sort);
        let num_cols = keys.len();
        let rows = hashmaps
            .into_iter()
            .map(|hm| {
                let mut vals = Vec::with_capacity(num_cols);
                for key in &keys {
                    vals.push(hm.get(key.as_str()).map(|cow| cow.to_string()));
                }
                vals
            })
            .collect();

        Ok((keys, rows))
    }
}

fn field_custom_sort(a: &String, b: &String) -> Ordering {
    if a == "source_timestamp" {
        Ordering::Less
    } else if b == "source_timestamp" {
        Ordering::Greater
    } else {
        a.cmp(b)
    }
}
