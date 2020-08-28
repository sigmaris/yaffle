use std::collections::{HashMap, HashSet};
use std::borrow::Cow;

use horrorshow::helper::doctype;
use horrorshow::{owned_html, Raw, Render};
use lazy_static::lazy_static;

use super::SearchParams;

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
static RELTIMES: &[(i32, &str)] = &[
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

pub(crate) fn base_page<'a>(content: impl Render + 'a) -> impl Render + 'a {
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
                // nav(class="navbar navbar-expand-sm navbar-dark bg-dark") {
                //     ul(class="navbar-nav") {
                //         li(class="nav-item") {
                //             a(class="nav-link active", href="#") { :"Search" }
                //         }
                //         li(class="nav-item") {
                //             a(class="nav-link", href="#") { :"Indexes" }
                //         }
                //     }
                // }
                :&content;
                script(src="https://code.jquery.com/jquery-3.5.1.slim.min.js", integrity="sha384-DfXdz2htPH0lsSSs5nCTpuj/zy4C+OGpamoFVy38MVBnE+IbbVYUew+OrCXaRkfj", crossorigin="anonymous");
                script(src="https://cdn.jsdelivr.net/npm/popper.js@1.16.0/dist/umd/popper.min.js", integrity="sha384-Q6E9RHvbIyZFJoft+2mJbHaEWldlvI9IOYy5n3zV9zzTtmI3UksdQRVvoxMfooAo", crossorigin="anonymous");
                script(src="https://stackpath.bootstrapcdn.com/bootstrap/4.5.0/js/bootstrap.min.js", integrity="sha384-OgVRvuATP1z7JjHLkuOU7Xw704+h835Lr+6QL9UvYjZE3Ipu6Tp75j7Bh/kR0JKI", crossorigin="anonymous");
            }
        }
    }
}

pub(crate) fn doc_list_content<'a>(
    sp: &'a SearchParams,
    fields: &'a [&'a str],
    results: &'a [HashMap<&'a str, Cow<str>>],
    alerts: &'a [String],
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
            @for alert in alerts {
                div(class="row") {
                    div(class="col-md-12") {
                        div(class="alert alert-warning alert-dismissible fade show", role="alert") {
                            :alert;
                            button(type="button", class="close", data-dismiss="alert", aria-label="Close") {
                                span(aria-hidden="true") {:Raw("&times;") }
                            }
                        }
                    }
                }
            }
            div(class="row") {
                nav(class="col-md-3 col-lg-2") {
                    div(class="sticky-top") {
                        div(class="nav") {
                            form {
                                fieldset(class="form-group") {
                                    legend { :"Fields" }
                                    @for field in fields {
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
                            @for doc in results {
                                tr {
                                    @for field in fields {
                                        td(class=format!("{}_col", field), style=if DEFAULT_FIELDS.contains(field) {""} else {"display: none"}) {
                                            :doc.get(field).map(|x| x.as_ref())
                                        }
                                    }
                                }
                                tr {
                                    td(colspan=fields.len(), class="border-top-0") { code { :doc.get("message").map(|x| x.as_ref()) } }
                                }
                                @if doc.get("full_message").is_some() {
                                    tr {
                                        td(colspan=fields.len(), class="border-top-0") { pre { code { :doc.get("full_message").map(|x| x.as_ref()) } } }
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
