use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::convert::TryInto;

use horrorshow::Template;
use hyper::http::{header, StatusCode};
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use toshi::{Client, HyperToshi, Query, Search};
use warp::{reply, Filter};

use crate::{query::parse_query, schema::Document, SharedSettings};

mod templates;

#[derive(Deserialize, Debug)]
pub(crate) struct SearchParams {
    reltime: i32,
    q: String,
}

fn default_search() -> Search {
    Search {
        query: Some(Query::All),
        facets: None,
        limit: 500, // TODO: custom limit
        sort_by: Some("source_timestamp".to_string()),
    }
}

fn search_route(
    settings: SharedSettings,
) -> impl warp::Filter<Extract = (String,), Error = warp::reject::Rejection> + Clone {
    warp::path("search")
        .and(warp::path::end())
        .and(warp::query())
        .and_then(move |sp: SearchParams| {
            let c = HyperToshi::with_client(
                &settings.lock().unwrap().toshi_url,
                hyper::Client::default(),
            );
            let index_name = settings.lock().unwrap().index_name();
            let mut warnings = vec![];
            let query = if sp.q != "" {
                parse_query(
                    sp.q.trim(),
                    if sp.reltime > 0 {
                        Some(sp.reltime.try_into().unwrap())
                    } else {
                        None
                    },
                )
                .map(|(_remaining, parsed)| parsed)
                .unwrap_or_else(|e| {
                    warnings.push(format!("Can't parse query: {}", e));
                    Query::All
                })
            } else {
                Query::All
            };
            async move {
                let mut response = c
                    .search::<&str, Document>(
                        &index_name,
                        Search {
                            query: Some(query),
                            facets: None,
                            limit: 500, // TODO: custom limit
                            sort_by: Some("source_timestamp".to_string()),
                        },
                    )
                    .await;
                if let Err(e) = response {
                    warnings.push(format!("Error from Toshi: {}", e));
                    response = c
                        .search::<&str, Document>(&index_name, default_search())
                        .await;
                }
                match response {
                    Ok(results) => {
                        let hashmaps = results
                            .get_docs()
                            .iter()
                            .map(|scored_doc| {
                                let doc: &Document = &scored_doc.doc;
                                doc.into()
                            })
                            .collect();
                        let fields = fieldset(&hashmaps);
                        Ok::<String, warp::reject::Rejection>(
                            templates::base_page(templates::doc_list_content(
                                &sp, &fields, &hashmaps, &warnings,
                            ))
                            .into_string()
                            .unwrap(),
                        )
                    }
                    Err(e) => Ok(format!("Error response from Toshi: {:?}", e)),
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

fn fieldset<'a>(results: &Vec<HashMap<&'a str, Cow<str>>>) -> Vec<&'a str> {
    let mut field_set = HashSet::new();
    results.iter().fold(&mut field_set, |mut set, doc| {
        doc.keys().fold(&mut set, |inner_set, item| {
            if *item != "message" && *item != "full_message" {
                inner_set.insert(item);
            }
            inner_set
        });
        set
    });
    let mut fields: Vec<&str> = field_set.drain().map(|s| (*s)).collect();
    fields.sort_by(field_custom_sort);
    fields
}

pub async fn run_http_server(
    settings: SharedSettings,
    http_listener: &'static mut TcpListener,
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
