use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{TimeZone, Utc};
use horrorshow::helper::doctype;
use horrorshow::{owned_html, Raw, Render, Template};
use hyper::http::{header, StatusCode};
use lazy_static::lazy_static;
use serde::Deserialize;
use serde_json::{Number, Value};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use toshi::{BoolQuery, Client, ExactTerm, HyperToshi, Query, RangeQuery, Search, SearchResults};
use warp::{reply, Filter};

use crate::{get_our_schema_map, FieldType, Settings};

lazy_static! {
    static ref DEFAULT_FIELDS: HashSet<&'static str> = {
        let mut hs = HashSet::new();
        hs.insert("source_timestamp");
        hs.insert("comm");
        hs.insert("hostname");
        hs.insert("priority");
        hs
    };
}
static RELTIMES: &'static [(i32, &'static str)] = &[
    (1, "1m"),
    (5, "5m"),
    (15, "15m"),
    (30, "30m"),
    (60, "1h"),
    (180, "2h"),
    (720, "8h"),
    (1440, "1d"),
    (2880, "2d"),
    (7200, "5d"),
    (10080, "7d"),
    (20160, "14d"),
    (43200, "30d"),
    (-1, "all"),
];

fn base_page<'a>(content: impl Render + 'a) -> impl Render + 'a {
    owned_html! {
        : doctype::HTML;
        html(lang="en") {
            head {
                meta(charset="utf-8");
                meta(name="viewport", content="width=device-width, initial-scale=1, shrink-to-fit=no");
                link(rel="stylesheet", href="https://stackpath.bootstrapcdn.com/bootstrap/4.5.0/css/bootstrap.min.css", integrity="sha384-9aIt2nRpC12Uk9gS9baDl411NQApFmC26EwAOH8WgZl5MYYxFfc+NcPb1dKGj7Sk", crossorigin="anonymous");
                title: "Yaffle";
            }
            body {
                nav(class="navbar navbar-expand-sm navbar-dark bg-dark") {
                    ul(class="navbar-nav") {
                        li(class="nav-item") {
                            a(class="nav-link active", href="#") { :"Search" }
                        }
                        li(class="nav-item") {
                            a(class="nav-link", href="#") { :"Indexes" }
                        }
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
    sp: &'a SearchParams,
    fields: &'a Vec<&'a str>,
    results: &'a SearchResults<HashMap<String, Value>>,
) -> impl Render + 'a {
    owned_html! {
        nav(class="navbar navbar-light flex-md-nowrap p-0") {
            form(action="search", method="GET", class="form-inline col-md-12") {
                select(class="custom-select custom-select-sm col-auto mx-1 my-2", name="reltime") {
                    @for option in RELTIMES {
                        option(value=option.0, selected?=(sp.reltime == option.0)) { :option.1 }
                    }
                }
                input(value=&sp.q, class="form-control form-control-sm col mx-1 my-2", name="q", type="text", placeholder="Search", aria-label="Search");
                button(class="btn btn-sm btn-outline-success mx-1 my-2 col-auto", type="submit") { : "Go"; }
            }
        }
        div(class="container-fluid") {
            div(class="row") {
                nav(class="col-md-3 col-lg-2") {
                    div(class="sticky-top") {
                        div(class="nav") {
                            form {
                                fieldset(class="form-group") {
                                    legend { :"Fields" }
                                    @for field in fields {
                                        @if field != &"source_timestamp" {
                                            div(class="form-check") {
                                                input(
                                                    class="form-check-input log-field-toggle",
                                                    type="checkbox",
                                                    value=field,
                                                    data-target=format!("{}_col", field),
                                                    id=format!("{}_check", field),
                                                    checked?=DEFAULT_FIELDS.contains(field)
                                                );
                                                label(class="form-check-label", for=format!("{}_check", field)) { :field }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                main(role="main", class="col-md-9 col-lg-10 px-0") {
                    table(class="table table-sm") {
                        thead(class="thead-light sticky-top") {
                            tr {
                                @for field in fields {
                                    th(scope="col", class=format!("{}_col", field), style=if DEFAULT_FIELDS.contains(field) {""} else {"display: none"}) { :field }
                                }
                            }
                        }
                        tbody {
                            @for scored_doc in results.get_docs() {
                                tr {
                                    @for field in fields {
                                        td(class=format!("{}_col", field), style=if DEFAULT_FIELDS.contains(field) {""} else {"display: none"}) {
                                            :format_value(scored_doc.doc.get(&field.to_string()), field)
                                        }
                                    }
                                }
                                tr {
                                    td(colspan=fields.len(), class="border-top-0") { code { :format_value(scored_doc.doc.get("message"), "message") } }
                                }
                                @if scored_doc.doc.get("full_message").is_some() {
                                    tr {
                                        td(colspan=fields.len(), class="border-top-0") { pre { code { :format_value(scored_doc.doc.get("full_message"), "full_message") } } }
                                    }
                                }
                            }
                        }
                    }
                }
                script { :Raw("document.querySelectorAll('.log-field-toggle').forEach((button) => {
                    button.addEventListener('change', (evt) => {
                        if (evt.target.checked) {
                            $('.' + evt.target.dataset['target']).show();
                        } else {
                            $('.' + evt.target.dataset['target']).hide();
                        }
                    });
                });") }
            }
        }
    }
}

fn lookup_severity(n: &Number) -> &'static str {
    match n.as_u64() {
        Some(0) => "Emergency",
        Some(1) => "Alert",
        Some(2) => "Critical",
        Some(3) => "Error",
        Some(4) => "Warning",
        Some(5) => "Notice",
        Some(6) => "Informational",
        Some(7) => "Debug",
        Some(_) => "Unknown",
        None => "None",
    }
}

fn add_reltime_to_query(q: Query, reltime: i32) -> Query {
    if reltime < 0 {
        q
    } else {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards!?");
        let start = now - Duration::from_secs(reltime as u64 * 60);
        let rq = RangeQuery::builder()
            .for_field("source_timestamp")
            .gte(Some(start.as_micros()))
            .build();
        if let Query::All = q {
            rq
        } else {
            BoolQuery::builder().must_match(rq).must_match(q).build()
        }
    }
}

fn format_value(val: Option<&Value>, field_name: &str) -> Result<String, std::io::Error> {
    let schema_map = get_our_schema_map();
    if let Some(v) = val {
        match (
            field_name,
            schema_map.get(field_name).map(|s| s.get_type()),
            v,
        ) {
            (_, Some(FieldType::Timestamp), Value::Number(n)) => n
                .as_u64()
                .map(|u| {
                    Utc.timestamp_opt((u / 1_000_000) as i64, ((u % 1_000_000) * 1000) as u32)
                        .single()
                        .map(|dt| format!("{}", dt))
                        .unwrap_or("".to_string())
                })
                .ok_or(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Can't get {} as u64", field_name),
                )),
            ("priority", Some(_), Value::Number(n)) => {
                Ok(format!("{} ({})", n, lookup_severity(n)))
            }
            ("cap_effective", Some(FieldType::U64), Value::Number(n)) => Ok(n
                .as_u64()
                .map(|num| format!("0x{:x}", num))
                .unwrap_or(n.to_string())),
            (_, Some(_), Value::Number(n)) => Ok(n.to_string()),
            (_, Some(_), Value::String(s)) => Ok(s.to_string()),
            (_, Some(_), other) => Ok(other.to_string()),
            (_, None, _) => Err(std::io::Error::new(
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

fn search_route(
    settings: Arc<Mutex<Settings>>,
) -> impl warp::Filter<Extract = (String,), Error = warp::reject::Rejection> + Clone {
    warp::path("search")
        .and(warp::path::end())
        .and(warp::query())
        .and_then(move |sp: SearchParams| {
            let c = HyperToshi::with_client(
                &settings.lock().unwrap().toshi_url,
                hyper::Client::default(),
            );
            let index_name = settings.lock().unwrap().index_name.clone();
            let query = if sp.q != "" {
                Query::Exact(ExactTerm::with_term("message", &sp.q))
            } else {
                Query::All
            };
            async move {
                let response = c
                    .search::<&str, HashMap<String, Value>>(
                        &index_name,
                        Search {
                            query: Some(add_reltime_to_query(query, sp.reltime)),
                            facets: None,
                            limit: 500, // TODO: custom limit
                            sort_by: Some("source_timestamp".to_string()),
                        },
                    )
                    .await;
                match response {
                    Ok(results) => {
                        let fields = fieldset(&results);
                        Ok::<String, warp::reject::Rejection>(
                            base_page(doc_list_content(&sp, &fields, &results))
                                .into_string()
                                .unwrap(),
                        )
                    }
                    Err(e) => Ok(format!("{:?}", e)),
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

fn fieldset(results: &SearchResults<HashMap<String, Value>>) -> Vec<&str> {
    let mut field_set = HashSet::new();
    results
        .get_docs()
        .iter()
        .fold(&mut field_set, |mut set, scored_doc| {
            scored_doc.doc.keys().fold(&mut set, |inner_set, item| {
                if item != "message" && item != "full_message" {
                    inner_set.insert(item);
                }
                inner_set
            });
            set
        });
    let mut fields: Vec<&str> = field_set.drain().map(|s| (*s).as_str()).collect();
    fields.sort_by(field_custom_sort);
    fields
}

pub async fn run_http_server(
    settings: Arc<Mutex<Settings>>,
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
