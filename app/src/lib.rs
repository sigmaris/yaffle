pub mod error_template;
pub mod settings;

use async_trait::async_trait;
use cookie::Cookie;
use leptos::*;
use leptos_meta::*;
use leptos_router::*;
use std::{
    any::TypeId,
    collections::HashSet,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

static RELTIMES: &[(u32, &str)] = &[
    (0, "all"),
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
];

static DEFAULT_COLS_ENABLED: [&str; 4] = ["source_timestamp", "comm", "hostname", "priority"];

pub type SharedServerAPI = Arc<dyn ServerAPI + Send + Sync>;

fn app_state_from_cookies<'a>(cookies_hdrs: impl Iterator<Item = &'a str>) -> AppState {
    for cookie in cookies_hdrs
        .flat_map(|val| val.split(';'))
        .filter_map(|ck| Cookie::parse_encoded(ck).ok())
    {
        if cookie.name() == "cols" {
            return AppState {
                cols_enabled: HashSet::from_iter(cookie.value().split(',').map(str::to_string)),
            };
        }
    }
    AppState::default()
}

#[cfg(feature = "ssr")]
fn extract_app_state(cx: Scope) -> AppState {
    use_context::<leptos_axum::RequestParts>(cx)
        .map(|req_parts| {
            app_state_from_cookies(
                req_parts
                    .headers
                    .get_all(http::header::COOKIE)
                    .into_iter()
                    .filter_map(|val| val.to_str().ok()),
            )
        })
        .unwrap_or_default()
}

#[cfg(not(feature = "ssr"))]
fn extract_app_state(cx: Scope) -> AppState {
    use wasm_bindgen::JsCast;
    web_sys::window()
        .map(|w| w.document())
        .flatten()
        .map(|d| d.dyn_into::<web_sys::HtmlDocument>().ok())
        .flatten()
        .map(|d| d.cookie().ok())
        .flatten()
        .map(|window_cookie| app_state_from_cookies([window_cookie].iter().map(|s| s.as_str())))
        .unwrap_or_default()
}

#[component]
pub fn App(cx: Scope, #[prop(optional)] server_api: Option<SharedServerAPI>) -> impl IntoView {
    // Provides context that manages stylesheets, titles, meta tags, etc.
    provide_meta_context(cx);

    // If we are running on the server, provide the server API
    if let Some(some_api) = server_api {
        provide_context(cx, some_api)
    }

    // Extract app state from cookies
    provide_context(cx, extract_app_state(cx));

    view! {
        cx,

        <Link rel="stylesheet" href="https://stackpath.bootstrapcdn.com/bootstrap/4.5.0/css/bootstrap.min.css" integrity="sha384-9aIt2nRpC12Uk9gS9baDl411NQApFmC26EwAOH8WgZl5MYYxFfc+NcPb1dKGj7Sk" crossorigin="anonymous"/>
        // injects a stylesheet into the document <head>
        // id=leptos means cargo-leptos will hot-reload this stylesheet
        <Stylesheet id="leptos" href="/pkg/yaffle.css"/>

        // sets the document title
        <Title text="Yaffle Web"/>

        <Router>
            <Routes>
                <Route path="" view=|cx| view! { cx, <SearchPage/> }/>
            </Routes>
        </Router>
    }
}

#[component]
fn SearchPage(cx: Scope) -> impl IntoView {
    let maybe_qstr = use_query::<SearchParams>(cx);
    let params_signal = move || {
        let mut p = maybe_qstr.get().unwrap_or_default();
        if p.q.as_deref() == Some("") {
            p.q = None;
        }
        (p.reltime.unwrap_or(5), p.q.unwrap_or("*".to_string()))
    };
    let app_state: AppState = use_context(cx).unwrap_or_default();
    let (cols_enabled, set_cols_enabled) = create_signal(cx, app_state.cols_enabled);
    create_effect(cx, move |_| {
        let cookie_list = cols_enabled
            .get()
            .iter()
            .map(|s| &**s)
            .collect::<Vec<&str>>()
            .join(",");
        use wasm_bindgen::JsCast;
        web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.dyn_into::<web_sys::HtmlDocument>().ok())
            .map(|d| d.set_cookie(format!("cols={}", cookie_list).as_str()));
    });
    let search_res = create_resource(cx, params_signal, move |value| async move {
        perform_search(cx, value.0, value.1).await
    });
    view! { cx,
        <nav class="navbar navbar-light flex-md-nowrap p-0">
            <Form method="GET" action="" class="form-inline col-md-12">
                <select name="reltime" class="custom-select custom-select-sm col-auto mx-1 my-2">
                    {RELTIMES.iter().map(|(val, label)|
                        view! { cx, <option selected=params_signal().0 == *val value=*val>{label.to_string()}</option> }
                    ).collect_view(cx)}
                </select>
                <input type="search" name="q" placeholder="Enter search query..."
                    value=move || maybe_qstr.get().unwrap_or_default().q.unwrap_or_default()
                    class="form-control form-control-sm col mx-1 my-2"/>
                <button type="submit" class="btn btn-sm btn-outline-success mx-1 my-2 col-auto">
                "Go"
                </button>
            </Form>
        </nav>
        <div class="container-fluid">
            <div class="row">
                <Transition fallback=move || view! { cx, <p>"Loading..."</p> }>
                    <ErrorBoundary fallback=|cx, errors| view! { cx,
                        <div class="row">
                            <div class="col-md-12">
                            "Errors:"
                            </div>
                        </div>
                        {move || errors.get()
                            .into_iter()
                            .map(|(_, e)| view! { cx,
                            <div class="alert alert-warning show" role="alert">
                                {e.to_string()}
                            </div>
                            }).collect_view(cx)
                        }
                    }>
                        <nav class="col-md-3 col-lg-2">
                            <div class="sticky-top">
                                <div class="nav">
                                    <form>
                                        <fieldset class="form-group">
                                            <legend>"Fields"</legend>
                                            {search_res.read(cx).map(|search| { search.map(|(fields, _rows)| {fields.iter().filter_map(|field| {
                                                if field != "message" && field != "full_message" {
                                                    Some(view! { cx, <div class="form-check">
                                                        <input
                                                            type="checkbox"
                                                            class="form-check-input log-field-toggle"
                                                            value=field
                                                            on:input=move |ev| {
                                                                let val = event_target_value(&ev);
                                                                if event_target_checked(&ev) != cols_enabled.get().contains(&val) {
                                                                    set_cols_enabled.update(|enabled_set| {
                                                                        if event_target_checked(&ev) {
                                                                            enabled_set.insert(val);
                                                                        } else {
                                                                            enabled_set.remove(&val);
                                                                        }
                                                                    });
                                                                }
                                                            }
                                                            id=format!("{}_check", field)
                                                            checked=cols_enabled.get().contains(field)/>
                                                        <label class="form-check-label" for=format!("{}_check", field)>{field}</label>
                                                    </div> })
                                                } else {
                                                    None
                                                }
                                            }).collect_view(cx)})})}
                                        </fieldset>
                                    </form>
                                </div>
                            </div>
                        </nav>
                        <main role="main" class="col-md-9 col-lg-10 px-0">
                            <table class="table table-sm">
                                <thead class="thead-light sticky-top">
                                    <tr>
                                        {search_res.read(cx).map(|result| result.map(|ok_res| ok_res.0.iter().filter_map(|field| {
                                            if field == "message" || field == "full_message" {
                                                None
                                            } else if cols_enabled.get().contains(field) {
                                                Some(view! {cx, <th scope="col">{field}</th>})
                                            } else {
                                                Some(view! {cx, <th scope="col" style:display="none"></th>})
                                            }
                                        }).collect_view(cx)))}
                                    </tr>
                                </thead>
                                <tbody>
                                    {search_res.read(cx).map(|result| result.map(|ok_res| ok_res.1.iter().map(|row| {view! {cx,
                                        <tr>
                                            {ok_res.0.iter().enumerate().filter_map(|(index, field)| {
                                                if field == "message" || field == "full_message" {
                                                    None
                                                } else if cols_enabled.get().contains(field) {
                                                    Some(view! {cx, <td>{row.get(index).unwrap_or(&None).clone()}</td>})
                                                } else {
                                                    Some(view! {cx, <td style:display="none"/>})
                                                }
                                            }).collect_view(cx)}
                                        </tr>
                                        <tr>
                                            <td colspan=ok_res.0.len() class="border-top-0">
                                                <code>{ok_res.0.iter().enumerate().filter_map(|(index, field)| if field == "message" {
                                                    row.get(index).unwrap_or(&None).clone()
                                                } else {
                                                    None
                                                }).collect_view(cx)}</code>
                                            </td>
                                        </tr>
                                    }}).collect_view(cx)))}
                                </tbody>
                            </table>
                        </main>
                    </ErrorBoundary>
                </Transition>
            </div>
        </div>
    }
}

#[derive(PartialEq, Params, Default, Debug, Clone)]
struct SearchParams {
    reltime: Option<u32>,
    q: Option<String>,
}

#[server(PerformSearch, "/api")]
pub async fn perform_search(
    cx: Scope,
    reltime: u32,
    query: String,
) -> Result<(Vec<String>, Vec<Vec<Option<String>>>), ServerFnError> {
    let server_api = if let Some(s) = use_context::<SharedServerAPI>(cx) {
        s
    } else {
        log!(
            "Missing SharedServerAPI with {:?} from context {:?}",
            TypeId::of::<SharedServerAPI>(),
            cx
        );
        return Err(ServerFnError::ServerError(
            "Missing SharedSettings".to_string(),
        ));
    };
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let start_timestamp = if reltime > 0 {
        now.checked_sub(Duration::from_secs((reltime * 60).into()))
            .map(|d| d.as_secs())
    } else {
        None
    };
    let params = SearchOptions {
        query,
        start_timestamp,
        end_timestamp: None,
    };
    server_api
        .search(&params)
        .await
        .map_err(ServerFnError::ServerError)
}

#[cfg(feature = "ssr")]
pub fn register_server_functions() {
    PerformSearch::register().unwrap();
}

#[derive(Clone, Debug)]
pub struct AppState {
    pub cols_enabled: HashSet<String>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            cols_enabled: HashSet::from_iter(DEFAULT_COLS_ENABLED.iter().map(|x| x.to_string())),
        }
    }
}

pub struct SearchOptions {
    pub query: String,
    pub start_timestamp: Option<u64>,
    pub end_timestamp: Option<u64>,
}

#[async_trait]
pub trait ServerAPI {
    async fn search(
        &self,
        params: &SearchOptions,
    ) -> Result<(Vec<String>, Vec<Vec<Option<String>>>), String>;
}

trait CloneableServerAPI: ServerAPI + Clone {}
