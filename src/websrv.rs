use chrono::{DateTime, TimeZone, Utc};
use horrorshow::{Render, RenderBox, RenderOnce};
use std::collections::{HashMap, HashSet};

use horrorshow::helper::doctype;
use horrorshow::Template;
use horrorshow::{box_html, html, owned_html};
use serde::Deserialize;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use toshi::{Client, ExactTerm, HyperToshi, Query, Search, SearchResults};
use warp::Filter;

use crate::{get_our_schema_map, FieldType};

fn base_page(content: impl Render) -> impl Render {
    owned_html! {
        : doctype::HTML;
        html(lang="en") {
            head {
                meta(charset="utf-8");
                meta(name="viewport", content="width=device-width, initial-scale=1, shrink-to-fit=no");
                link(rel="stylesheet", href="https://stackpath.bootstrapcdn.com/bootstrap/4.5.0/css/bootstrap.min.css", integrity="sha384-9aIt2nRpC12Uk9gS9baDl411NQApFmC26EwAOH8WgZl5MYYxFfc+NcPb1dKGj7Sk", crossorigin="anonymous");
                title: "Rustylog";
            }
            body {
                nav(class="navbar navbar-light bg-light sticky-top flex-md-nowrap p-0 shadow") {
                    form(action="search", method="GET", class="form-inline col-md-12") {
                        select(class="custom-select custom-select-sm col-auto mx-1 my-2", name="reltime") {
                            option(value="5") { :"5m"; }
                            option(value="15") { :"15m"; }
                            option(value="30") { :"30m"; }
                            option(value="60") { :"1h"; }
                            option(value="180") { :"2h"; }
                            option(value="720") { :"8h"; }
                            option(value="1440") { :"1d"; }
                            option(value="2880") { :"2d"; }
                            option(value="7200") { :"5d"; }
                            option(value="10080") { :"7d"; }
                            option(value="20160") { :"14d"; }
                            option(value="43200") { :"30d"; }
                            option(value="-1") { :"all"; }
                        }
                        input(class="form-control form-control-sm col mx-1 my-2", name="q", type="text", placeholder="Search", aria-label="Search");
                        button(class="btn btn-sm btn-outline-success mx-1 my-2 col-auto", type="submit") { : "Go"; }
                    }
                }
                :&content;
                script(src="https://code.jquery.com/jquery-3.5.1.slim.min.js", integrity="sha384-DfXdz2htPH0lsSSs5nCTpuj/zy4C+OGpamoFVy38MVBnE+IbbVYUew+OrCXaRkfj", crossorigin="anonymous");
                script(src="https://cdn.jsdelivr.net/npm/popper.js@1.16.0/dist/umd/popper.min.js", integrity="sha384-Q6E9RHvbIyZFJoft+2mJbHaEWldlvI9IOYy5n3zV9zzTtmI3UksdQRVvoxMfooAo", crossorigin="anonymous");
                script(src="https://stackpath.bootstrapcdn.com/bootstrap/4.5.0/js/bootstrap.min.js", integrity="sha384-OgVRvuATP1z7JjHLkuOU7Xw704+h835Lr+6QL9UvYjZE3Ipu6Tp75j7Bh/kR0JKI", crossorigin="anonymous");
            }
        }
    }
}

fn doc_list_content<'a>(
    fields: &'a Vec<&'a str>,
    results: &'a SearchResults<HashMap<String, Value>>,
) -> impl Render + 'a {
    owned_html! {
        table(class="table table-sm") {
            thead(class="thead-dark") {
                tr {
                    @for field in fields {
                        @if field != &"message" {
                            th(scope="col") { :field }
                        }
                    }
                }
            }
            tbody {
                @for scored_doc in results.get_docs() {
                    tr {
                        @for field in fields {
                            @if field != &"message" {
                                td { :format_value(scored_doc.doc.get(&field.to_string()), field) }
                            }
                        }
                    }
                    tr {
                        td(colspan=fields.len()-1) { :format_value(scored_doc.doc.get("message"), "message") }
                    }
                }
            }
        }
    }
}

fn format_value(val: Option<&Value>, field_name: &str) -> Result<String, std::io::Error> {
    let schema_map = get_our_schema_map();
    if let Some(v) = val {
        match (schema_map.get(field_name).map(|s| s.get_type()), v) {
            (Some(FieldType::Timestamp), Value::Number(n)) => n
                .as_f64()
                .map(|f| {
                    format!(
                        "{:?} ({})",
                        Utc.timestamp_opt(f.trunc() as i64, (f.fract() * 1e+9 as f64) as u32),
                        f
                    )
                })
                .ok_or(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Can't get {} as f64", field_name),
                )),
            (Some(_), Value::Number(n)) => Ok(n.to_string()),
            (Some(_), Value::String(s)) => Ok(s.to_string()),
            (Some(_), other) => Ok(other.to_string()),
            (None, _) => Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("No such field named {} in schema", field_name),
            )),
        }
    } else {
        Ok("".to_string())
    }
}

#[derive(Deserialize, Debug)]
struct SearchParams {
    reltime: i32,
    q: String,
}

fn search_route<'a>(
    toshi_srv_url: &'a str,
) -> impl warp::Filter<Extract = (String,), Error = warp::reject::Rejection> + Clone + 'a {
    warp::path("search")
        .and(warp::path::end())
        .and(warp::query())
        .and_then(move |sp: SearchParams| {
            let hc = hyper::Client::default();
            let c = HyperToshi::with_client(toshi_srv_url, hc);
            async move {
                let response = c
                    .search::<&str, HashMap<String, Value>>(
                        "rustylog_0",
                        Search {
                            query: Some(Query::Exact(ExactTerm::with_term("hostname", sp.q))),
                            facets: None,
                            limit: 50, // TODO: custom limit
                            sort_by: Some("source_timestamp".to_string()),
                        },
                    )
                    .await;
                match response {
                    Ok(results) => {
                        let fields = fieldset(&results);
                        Ok::<String, warp::reject::Rejection>(
                            base_page(doc_list_content(&fields, &results))
                                .into_string()
                                .unwrap(),
                        )
                    }
                    Err(e) => Ok(format!("{}", e)),
                }
            }
        })
}

fn fieldset(results: &SearchResults<HashMap<String, Value>>) -> Vec<&str> {
    let mut field_set = HashSet::new();
    results
        .get_docs()
        .iter()
        .fold(&mut field_set, |mut set, scored_doc| {
            scored_doc.doc.keys().fold(&mut set, |inner_set, item| {
                inner_set.insert(item);
                inner_set
            });
            set
        });
    let mut fields: Vec<&str> = field_set.drain().map(|s| (*s).as_str()).collect();
    fields.sort();
    fields
}

pub async fn run_http_server(
    http_listener: &'static mut TcpListener,
    shutdown_rx: oneshot::Receiver<()>,
) {
    let client = hyper::Client::default();
    let c = HyperToshi::with_client("http://localhost:8080", client);
    let search_route = search_route("http://localhost:8080");
    let root_route = warp::path::end().and_then(move || {
        let cli = c.clone();
        async move {
            match cli
                .search::<&str, HashMap<String, Value>>(
                    "rustylog_0",
                    Search {
                        query: Some(Query::All),
                        facets: None,
                        limit: 50, // TODO: custom limit
                        sort_by: Some("source_timestamp".to_string()),
                    },
                )
                .await
            {
                Ok(results) => {
                    let fields = fieldset(&results);
                    Ok::<String, warp::reject::Rejection>(
                        base_page(doc_list_content(&fields, &results))
                            .into_string()
                            .unwrap(),
                    )
                }
                Err(e) => Ok(format!("{}", e)),
            }
        }
    });
    let routes = search_route
        .or(root_route)
        .with(warp::reply::with::header("Content-Type", "text/html"));
    warp::serve(routes)
        .serve_incoming_with_graceful_shutdown(http_listener, async {
            shutdown_rx.await.ok();
        })
        .await;
}
