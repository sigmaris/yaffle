use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::iter::FromIterator;
use std::str::FromStr;
use std::time::Duration;

use horrorshow::Template;
use hyper::client::HttpConnector;
use hyper::http::{header, StatusCode};
use hyper::{Client, Uri};
use log::debug;
use serde::Deserialize;
use tokio::sync::oneshot;
use tokio_stream::wrappers::TcpListenerStream;
use url::Url;
use warp::{reply, Filter};

use crate::quickwit::SearchResults;
use crate::YaffleError;
use crate::{schema::Document, SharedSettings};

mod templates;

#[derive(Deserialize, Debug)]
pub(crate) struct SearchParams {
    reltime: i32,
    q: String,
}

fn build_search_uri(settings: &SharedSettings, sp: &SearchParams) -> Result<Uri, YaffleError> {
    let locked = settings.lock().unwrap();
    let mut search_url = Url::parse(&locked.quickwit_url)?;
    search_url
        .path_segments_mut()
        .unwrap()
        .extend(["api", "v1", &locked.quickwit_index, "search"]);
    search_url
        .query_pairs_mut()
        .append_pair("query", if sp.q.is_empty() { "*" } else { &sp.q })
        .append_pair("sort_by_field", "-source_timestamp");
    debug!("Search URI: {}", search_url.as_str());
    Ok(Uri::from_str(search_url.as_str())?)
}

fn search_result_with_error(error: impl ToString) -> SearchResults {
    SearchResults {
        elapsed_time_micros: 0,
        errors: vec![error.to_string()],
        hits: vec![],
        num_hits: 0,
    }
}

fn search_route(
    settings: SharedSettings,
) -> impl warp::Filter<Extract = (String,), Error = warp::reject::Rejection> + Clone {
    warp::path("search")
        .and(warp::path::end())
        .and(warp::query())
        .and_then(move |sp: SearchParams| {
            let client: Client<HttpConnector> = hyper::Client::builder()
                .pool_idle_timeout(Duration::from_secs(30))
                .build_http::<hyper::Body>();
            let shared_settings = settings.clone();
            async move {
                match build_search_uri(&shared_settings, &sp) {
                    Ok(search_uri) => {
                        let result = client.get(search_uri).await;
                        match result {
                            Ok(mut response) => {
                                let sr: SearchResults = hyper::body::to_bytes(response.body_mut())
                                    .await
                                    .map(|body_bytes| {
                                        serde_json::from_slice(&body_bytes)
                                            .unwrap_or_else(search_result_with_error)
                                    })
                                    .unwrap_or_else(search_result_with_error);
                                let hashmaps: Vec<_> =
                                    sr.hits.iter().map(|doc| doc.into()).collect();
                                let fields = fieldset(&hashmaps);
                                Ok::<String, warp::reject::Rejection>(
                                    templates::base_page(templates::doc_list_content(
                                        &sp, &fields, &hashmaps, &sr.errors,
                                    ))
                                    .into_string()
                                    .unwrap(),
                                )
                            }
                            Err(e) => Ok(format!("Error response from Toshi: {:?}", e)),
                        }
                    }
                    Err(uri_error) => Ok(format!("Error building search URI: {}", uri_error)),
                }
            }
        })
}

fn field_custom_sort(a: &&str, b: &&str) -> Ordering {
    if a == &"source_timestamp" {
        Ordering::Less
    } else if b == &"source_timestamp" {
        Ordering::Greater
    } else {
        a.cmp(b)
    }
}

fn fieldset<'a>(results: &[HashMap<&'a str, Cow<str>>]) -> Vec<&'a str> {
    let mut field_set = HashSet::new();
    results.iter().fold(&mut field_set, |mut set, doc| {
        doc.keys().fold(&mut set, |inner_set, item| {
            if *item != "message" && *item != "full_message" {
                inner_set.insert(*item);
            }
            inner_set
        });
        set
    });
    let mut fields = Vec::from_iter(field_set);
    fields.sort_by(field_custom_sort);
    fields
}

pub async fn run_http_server(
    settings: SharedSettings,
    http_listener: TcpListenerStream,
    shutdown_rx: oneshot::Receiver<()>,
) {
    let search_route = search_route(settings);
    let root_route = warp::path::end()
        .map(|| reply::with_header(StatusCode::FOUND, header::LOCATION, "/search?q=&reltime=5"));
    let routes = search_route
        .or(root_route)
        .with(warp::reply::with::header("Content-Type", "text/html"));
    warp::serve(routes)
        .serve_incoming_with_graceful_shutdown(http_listener, async {
            shutdown_rx.await.ok();
        })
        .await;
}
