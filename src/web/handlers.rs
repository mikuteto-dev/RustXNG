//! Request handlers, ported from the view functions of `searx/webapp.py`.

use axum::Form;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use super::limiter::{ClientAddr, LimiterVerdict};
use super::preferences::{Preferences, join_list, split_list};
use super::{
    AboutTemplate, AppState, ErrorTemplate, IndexTemplate, PreferencesTemplate, ResultView,
    ResultsTemplate, StatsTemplate, category_views, engine_views, render, with_cookies,
};
use crate::aggregator::{SearchRequest, SearchResponse};
use crate::engine::util::encode;
use crate::plugins::UserInfo;
use crate::query::{Query as SearchQuery, TimeRange};
use crate::version::VERSION;

/// Every query string parameter of `/search`, like upstream's `request.form`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SearchParams {
    pub q: Option<String>,
    pub category: Option<String>,
    pub categories: Option<String>,
    pub engine: Option<String>,
    pub engines: Option<String>,
    pub language: Option<String>,
    pub locale: Option<String>,
    pub pageno: Option<u32>,
    pub safesearch: Option<u8>,
    pub time_range: Option<String>,
    pub format: Option<String>,
    pub timeout_limit: Option<f64>,
    pub theme: Option<String>,
    pub autocomplete: Option<String>,
    pub enabled_engines: Option<String>,
    pub disabled_engines: Option<String>,
    pub enabled_plugins: Option<String>,
    pub disabled_plugins: Option<String>,
    pub image_proxy: Option<String>,
    /// \`full=1\` returns the rich JSON with timings and counters.
    pub full: Option<String>,
    /// Agent-facing extension: keep at most this many results.
    pub max_results: Option<usize>,
    /// Agent-facing extension: keep only these domains (comma separated).
    pub include_domains: Option<String>,
    /// Agent-facing extension: drop these domains (comma separated).
    pub exclude_domains: Option<String>,
    /// Agent-facing extension: fetch and return the page text of the top results.
    pub include_raw_content: Option<String>,
    /// Re-rank with the in-process EmbeddingGemma 2 model.
    pub semantic: Option<String>,
    /// Engine start strategy: "parallel" (default) or "staged".
    pub strategy: Option<String>,
}

/// Parameters of `/autocompleter`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AutocompleteParams {
    pub q: Option<String>,
    pub backend: Option<String>,
    pub locale: Option<String>,
}

/// `GET /` — the search form, or a search when `q` is set.
pub async fn index(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
    peer: ClientAddr,
    headers: HeaderMap,
) -> Response {
    if params
        .q
        .as_deref()
        .map(|query| !query.trim().is_empty())
        .unwrap_or(false)
    {
        return run_search(state, params, headers, peer).await;
    }
    search_form(&state, &headers, None)
}

/// `GET /search`.
pub async fn search_get(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
    peer: ClientAddr,
    headers: HeaderMap,
) -> Response {
    run_search(state, params, headers, peer).await
}

/// `POST /search`.
pub async fn search_post(
    State(state): State<AppState>,
    peer: ClientAddr,
    headers: HeaderMap,
    Form(params): Form<SearchParams>,
) -> Response {
    run_search(state, params, headers, peer).await
}

fn search_form(state: &AppState, headers: &HeaderMap, error: Option<String>) -> Response {
    let preferences = preferences_of(headers, state);
    let categories = category_views(&state.searcher, &preferences.categories);
    render(IndexTemplate {
        instance_name: state.settings().general.instance_name.clone(),
        version: VERSION.to_string(),
        query: String::new(),
        categories,
        engines: engine_views(&state.searcher),
        method: preferences.method,
        autocomplete: preferences.autocomplete,
        autocomplete_min: state.settings().search.autocomplete_min,
        has_error: error.is_some(),
        error: error.unwrap_or_default(),
        engine_count: state.searcher.registry.len(),
        time_range: String::new(),
        time_ranges: super::time_range_options(),
    })
}

/// Run a search and render it in the requested format.
async fn run_search(
    state: AppState,
    params: SearchParams,
    headers: HeaderMap,
    peer: ClientAddr,
) -> Response {
    let searcher = state.searcher.clone();
    let settings = searcher.settings.clone();
    let preferences = preferences_of(&headers, &state);

    let format = params
        .format
        .clone()
        .filter(|format| !format.is_empty())
        .unwrap_or_else(|| "html".to_string());
    if !settings.search.formats.contains(&format) {
        return error_page(
            &state,
            StatusCode::BAD_REQUEST,
            &format!("format {:?} is not enabled on this instance", format),
        );
    }

    let raw_query = params.q.clone().unwrap_or_default();
    if raw_query.trim().is_empty() {
        return error_page(&state, StatusCode::BAD_REQUEST, "missing query");
    }

    let mut query = SearchQuery::parse(
        &raw_query,
        &searcher.registry,
        &preferences.disabled_engines,
    );
    query.locale = params
        .locale
        .clone()
        .filter(|value| !value.is_empty())
        .or_else(|| Some(preferences.locale.clone()).filter(|value| !value.is_empty()))
        .unwrap_or_default();
    query.pageno = params.pageno.unwrap_or(1).max(1);
    query.time_range = params.time_range.as_deref().and_then(TimeRange::parse);
    query.safesearch = params.safesearch.unwrap_or(preferences.safesearch).min(2);
    query.timeout_limit = params.timeout_limit;

    let categories = split_param(params.categories.as_deref())
        .or_else(|| split_param(params.category.as_deref()))
        .unwrap_or_else(|| preferences.categories.clone());
    let engines = split_param(params.engines.as_deref())
        .or_else(|| split_param(params.engine.as_deref()))
        .unwrap_or_default();

    let max_page = if settings.search.max_page > 0 {
        settings.search.max_page
    } else {
        preferences.max_page
    };
    if max_page > 0 && query.pageno > max_page {
        return error_page(
            &state,
            StatusCode::BAD_REQUEST,
            &format!("page number is out of range: {max_page} is the maximum number of pages"),
        );
    }

    let enabled_plugins = split_param(params.enabled_plugins.as_deref())
        .unwrap_or_else(|| preferences.enabled_plugins.clone());
    let disabled_plugins = split_param(params.disabled_plugins.as_deref())
        .unwrap_or_else(|| preferences.disabled_plugins.clone());
    let language = if preferences.language.is_empty() {
        query.engine_language()
    } else {
        preferences.language.clone()
    };

    let user = user_info(&headers);
    match state.limiter.check(peer.0, &headers, &format) {
        LimiterVerdict::Allow => {}
        LimiterVerdict::Blocked {
            retry_after,
            message,
        } => return limiter_response(&state, retry_after, &message, None),
        LimiterVerdict::Challenge {
            retry_after,
            message,
            set_cookie,
        } => return limiter_response(&state, retry_after, &message, Some(&set_cookie)),
        LimiterVerdict::Redirect => return Redirect::to("/").into_response(),
    }

    let request = SearchRequest {
        query: query.clone(),
        pageno: query.pageno,
        categories: categories.clone(),
        engines: engines.clone(),
        disabled_engines: preferences.disabled_engines.clone(),
        time_range: query.time_range,
        safesearch: query.safesearch,
        language,
        timeout_limit: query.timeout_limit,
        enabled_plugins,
        disabled_plugins,
        locale: query.locale.clone(),
        user,
        max_results: params.max_results.map(|limit| limit.clamp(1, 200)),
        include_domains: split_param(params.include_domains.as_deref()).unwrap_or_default(),
        exclude_domains: split_param(params.exclude_domains.as_deref()).unwrap_or_default(),
        include_raw_content: params
            .include_raw_content
            .as_deref()
            .map(|value| matches!(value, "1" | "true" | "on" | "yes"))
            .unwrap_or(false),
        semantic: params
            .semantic
            .as_deref()
            .map(|value| matches!(value, "1" | "true" | "on" | "yes"))
            .unwrap_or(false),
        site_retry_done: false,
        strategy: match params.strategy.as_deref() {
            Some("staged") => crate::settings::SearchStrategy::Staged,
            Some("parallel") => crate::settings::SearchStrategy::Parallel,
            _ => settings.search.strategy,
        },
    };

    let response = searcher.search(request).await;
    if let Some(error) = &response.error {
        return error_page(&state, StatusCode::BAD_REQUEST, error);
    }
    if let Some(url) = response.redirect_url.clone() {
        return Redirect::to(&url).into_response();
    }

    match format.as_str() {
        "json" => {
            let full = params
                .full
                .as_deref()
                .map(|value| matches!(value, "1" | "true" | "on" | "yes"))
                .unwrap_or(false);
            if full {
                axum::Json(super::feeds::json_response_full(&response)).into_response()
            } else {
                axum::Json(super::feeds::upstream_json(&response)).into_response()
            }
        }
        "csv" => super::feeds::csv_response(&response),
        "rss" => super::feeds::rss_response(&state, &response, &raw_query),
        _ => results_page(
            &state,
            &preferences,
            &response,
            &params,
            &categories,
            &engines,
        ),
    }
}

fn results_page(
    state: &AppState,
    preferences: &Preferences,
    response: &SearchResponse,
    params: &SearchParams,
    categories: &[String],
    engines: &[String],
) -> Response {
    let base = state.base_url();
    let query = response.query.clone();
    let category = categories.join(",");
    let engine = engines.join(",");
    let language = params.language.clone().unwrap_or_default();
    let time_range = params.time_range.clone().unwrap_or_default();
    let pageno = response.pageno.max(1);

    let mut base_params: Vec<(&str, String)> = vec![("q", query.clone())];
    if !category.is_empty() {
        base_params.push(("category", category));
    }
    if !engine.is_empty() {
        base_params.push(("engine", engine));
    }
    if !language.is_empty() {
        base_params.push(("language", language));
    }
    if !time_range.is_empty() {
        base_params.push(("time_range", time_range));
    }
    let count = response.results.len().max(1) as u32;
    let page_of = |page: u32| {
        let mut pairs: Vec<(&str, String)> = base_params.clone();
        pairs.push(("pageno", page.to_string()));
        let query = pairs
            .iter()
            .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
            .collect::<Vec<_>>()
            .join("&");
        format!("{base}/search?{query}")
    };
    let prev_url = (pageno > 1).then(|| page_of(pageno - 1));
    let next_url = (response.paging && !response.results.is_empty())
        .then(|| page_of(pageno + 1))
        .filter(|_| count > 0 && response.paging);

    let timings: Vec<(String, String)> = response
        .timings
        .iter()
        .map(|timing| {
            (
                timing.engine.clone(),
                format!("{:.0} ms", timing.total * 1000.0),
            )
        })
        .collect();
    let unresponsive: Vec<String> = response
        .unresponsive_engines
        .iter()
        .map(|engine| {
            if engine.suspended {
                format!("{}: {} (suspended)", engine.engine, engine.error_type)
            } else {
                format!("{}: {}", engine.engine, engine.error_type)
            }
        })
        .collect();

    let mut results: Vec<ResultView> = response
        .results
        .iter()
        .map(ResultView::from_result)
        .collect();
    if preferences.image_proxy {
        let secret = &state.settings().server.secret_key;
        for result in &mut results {
            if !result.img_src.is_empty() {
                result.img_src_proxy =
                    super::image_proxy::proxied_url(&base, secret, &result.img_src);
            }
            if !result.thumbnail.is_empty() {
                result.thumbnail_proxy =
                    super::image_proxy::proxied_url(&base, secret, &result.thumbnail);
            }
        }
    }
    let infoboxes: Vec<super::InfoboxView> = response
        .infoboxes
        .iter()
        .map(super::InfoboxView::from_infobox)
        .collect();
    let engine_data: Vec<(String, Vec<(String, String)>)> = response
        .engine_data
        .iter()
        .map(|(engine, data)| {
            (
                engine.clone(),
                data.iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
            )
        })
        .collect();

    render(ResultsTemplate {
        instance_name: state.settings().general.instance_name.clone(),
        version: VERSION.to_string(),
        query: response.search_query.clone(),
        has_results: !results.is_empty(),
        results,
        has_answers: !response.answers.is_empty(),
        answers: response.answers.clone(),
        has_corrections: !response.corrections.is_empty(),
        corrections: response.corrections.clone(),
        has_suggestions: !response.suggestions.is_empty(),
        suggestions: response.suggestions.clone(),
        has_infoboxes: !infoboxes.is_empty(),
        infoboxes,
        has_unresponsive: !unresponsive.is_empty(),
        unresponsive_engines: unresponsive,
        number_of_results: response.number_of_results,
        pageno,
        has_prev: prev_url.is_some(),
        has_next: next_url.is_some(),
        prev_url: prev_url.unwrap_or_default(),
        next_url: next_url.unwrap_or_default(),
        engine_count: state.searcher.registry.len(),
        categories: category_views(&state.searcher, categories),
        engines: engine_views(&state.searcher),
        method: preferences.method.clone(),
        autocomplete: preferences.autocomplete.clone(),
        autocomplete_min: state.settings().search.autocomplete_min,
        time_range: params.time_range.clone().unwrap_or_default(),
        time_ranges: super::time_range_options(),
        has_timings: !timings.is_empty(),
        timings,
        has_engine_data: !engine_data.is_empty(),
        engine_data,
    })
}

/// `GET /autocompleter`.
pub async fn autocompleter(
    State(state): State<AppState>,
    Query(params): Query<AutocompleteParams>,
    headers: HeaderMap,
) -> Response {
    let query = params.q.clone().unwrap_or_default();
    if query.trim().is_empty() {
        return axum::Json(Vec::<String>::new()).into_response();
    }
    let preferences = preferences_of(&headers, &state);
    let parsed = SearchQuery::parse(
        &query,
        &state.searcher.registry,
        &preferences.disabled_engines,
    );
    if !parsed.autocomplete_list.is_empty() {
        return axum::Json(parsed.autocomplete_list).into_response();
    }
    let backend = params
        .backend
        .clone()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| preferences.autocomplete.clone());
    if backend == "none" || backend.is_empty() {
        return axum::Json(Vec::<String>::new()).into_response();
    }
    let locale = params
        .locale
        .clone()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| preferences.locale.clone());
    let mut suggestions = Vec::new();
    for candidate in backend.split(',') {
        let candidate = candidate.trim();
        if candidate.is_empty() {
            continue;
        }
        suggestions = crate::autocomplete::suggest(
            &state.searcher.network,
            state.settings(),
            candidate,
            &query,
            &locale,
        )
        .await;
        if !suggestions.is_empty() {
            break;
        }
    }
    axum::Json(suggestions).into_response()
}

/// `GET /config`.
pub async fn config(State(state): State<AppState>) -> Response {
    let settings = state.settings();
    let engines: Vec<serde_json::Value> = state
        .searcher
        .registry
        .iter()
        .map(|engine| {
            serde_json::json!({
                "name": engine.name(),
                "categories": engine.categories(),
                "shortcut": engine.shortcut(),
                "enabled": true,
                "paging": engine.paging(),
                "language_support": engine.language_support(),
                // the engine trait does not track per engine languages/regions
                "languages": [],
                "regions": [],
                "safesearch": engine.safesearch_support(),
                "time_range_support": engine.time_range_support(),
                "timeout": engine.timeout().as_secs_f64(),
            })
        })
        .collect();
    let plugins: Vec<serde_json::Value> = state
        .searcher
        .plugins
        .infos()
        .into_iter()
        .map(|plugin| {
            let enabled = state.searcher.plugins.is_enabled(
                &plugin.id,
                &settings.plugins.enabled,
                &settings.plugins.disabled,
            );
            serde_json::json!({ "name": plugin.id, "enabled": enabled })
        })
        .collect();
    let locales: serde_json::Map<String, serde_json::Value> = crate::locales::LOCALES
        .iter()
        .map(|locale| {
            let label = if locale.country.is_empty() {
                format!("{} ({})", locale.native, locale.english)
            } else {
                format!("{} ({}, {})", locale.native, locale.english, locale.country)
            };
            (locale.id.to_string(), serde_json::Value::String(label))
        })
        .collect();
    let brand = serde_json::json!({
        "CONTACT_URL": settings.general.contact_url.clone(),
        "DOCS_URL": "https://github.com/sahenjp/RustXNG/tree/main/docs",
        "GIT_URL": "https://github.com/sahenjp/RustXNG",
        "GIT_BRANCH": "main",
        "ISSUE_URL": "https://github.com/sahenjp/RustXNG/issues",
        "NEW_ISSUE_URL": "https://github.com/sahenjp/RustXNG/issues/new/choose",
        "PRIVACYPOLICY_URL": settings.general.privacypolicy_url.clone(),
        "DONATION_URL": settings.general.donation_url.clone(),
    });
    // upstream's /config is the contract used by clients; keep the field set
    let body = serde_json::json!({
        "version": crate::version::version_string(),
        "instance_name": settings.general.instance_name,
        "categories": state.searcher.registry.categories(),
        "engines": engines,
        "plugins": plugins,
        "locales": locales,
        "default_locale": settings.ui.default_locale,
        "autocomplete": settings.search.autocomplete,
        "safe_search": settings.search.safe_search,
        "default_theme": settings.ui.default_theme,
        "brand": brand,
        "limiter": { "enabled": state.limiter.is_enabled() },
        "doi_resolvers": [],
        "default_doi_resolver": "",
        "public_instance": settings.is_public_instance(),
    });
    axum::Json(body).into_response()
}

pub async fn preferences_get(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let settings = state.settings();
    let preferences = preferences_of(&headers, &state);
    let categories = category_views(&state.searcher, &preferences.categories);
    let locales: Vec<(String, String)> = crate::locales::LOCALES
        .iter()
        .map(|locale| {
            if locale.country.is_empty() {
                (locale.id.to_string(), locale.english.to_string())
            } else {
                (
                    locale.id.to_string(),
                    format!("{} ({})", locale.english, locale.country),
                )
            }
        })
        .collect();
    let disabled_engines = super::engine_list(&state.searcher, &preferences.disabled_engines);
    let enabled_plugins = join_list(&preferences.enabled_plugins);
    let disabled_plugins = join_list(&preferences.disabled_plugins);
    let engines = super::engine_views_for(&state.searcher, &preferences.disabled_engines);
    let plugins = super::plugin_views(
        &state.searcher.plugins,
        &preferences.enabled_plugins,
        &preferences.disabled_plugins,
    );
    let languages = super::select_options(
        std::iter::once((String::new(), String::new()))
            .chain(
                settings
                    .search
                    .languages
                    .iter()
                    .map(|language| (language.clone(), language.clone())),
            )
            .collect(),
        &preferences.language,
    );
    let locales = super::select_options(
        std::iter::once((String::new(), String::new()))
            .chain(locales)
            .collect(),
        &preferences.locale,
    );
    let themes = super::select_options(
        ["simple", "auto", "dark", "black"]
            .iter()
            .map(|theme| (theme.to_string(), theme.to_string()))
            .collect(),
        &preferences.theme,
    );
    let safesearch_options = super::select_options(
        vec![
            ("0".to_string(), "Off".to_string()),
            ("1".to_string(), "Moderate".to_string()),
            ("2".to_string(), "Strict".to_string()),
        ],
        &preferences.safesearch.to_string(),
    );
    let autocomplete_options = super::select_options(
        vec![
            ("none".to_string(), "Off".to_string()),
            ("duckduckgo".to_string(), "DuckDuckGo".to_string()),
            ("google".to_string(), "Google".to_string()),
            ("wikipedia".to_string(), "Wikipedia".to_string()),
            ("mwmbl".to_string(), "Mwmbl".to_string()),
        ],
        &preferences.autocomplete,
    );
    let method_options = super::select_options(
        vec![
            ("POST".to_string(), "POST".to_string()),
            ("GET".to_string(), "GET".to_string()),
        ],
        &preferences.method,
    );
    render(PreferencesTemplate {
        instance_name: settings.general.instance_name.clone(),
        version: VERSION.to_string(),
        preferences,
        categories,
        engines,
        plugins,
        languages,
        locales,
        themes,
        safesearch_options,
        autocomplete_options,
        method_options,
        formats: settings.search.formats.clone(),
        disabled_engines,
        enabled_plugins,
        disabled_plugins,
    })
}

/// `POST /preferences`.
pub async fn preferences_post(
    State(state): State<AppState>,
    _headers: HeaderMap,
    body: Bytes,
) -> Response {
    let settings = state.settings();
    let form = parse_form(&String::from_utf8_lossy(&body));
    let value = |key: &str| {
        form.iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
    };
    let values = |key: &str| -> Vec<String> {
        form.iter()
            .filter(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
            .collect()
    };

    if value("action").as_deref() == Some("reset") {
        let defaults = Preferences::from_settings(settings);
        return with_cookies(
            Redirect::to("/preferences").into_response(),
            &defaults.to_cookies(),
        );
    }

    let mut preferences = Preferences::from_settings(settings);
    if let Some(language) = value("language") {
        preferences.language = language;
    }
    if let Some(locale) = value("locale") {
        preferences.locale = locale;
    }
    if let Some(safesearch) = value("safesearch").and_then(|value| value.parse::<u8>().ok()) {
        preferences.safesearch = safesearch.min(2);
    }
    if let Some(theme) = value("theme") {
        preferences.theme = theme;
    }
    if let Some(method) = value("method") {
        preferences.method = method;
    }
    if let Some(autocomplete) = value("autocomplete") {
        preferences.autocomplete = autocomplete;
    }
    if value("image_proxy").is_some() {
        preferences.image_proxy = true;
    } else if form.iter().any(|(name, _)| name == "image_proxy_off") {
        preferences.image_proxy = false;
    }
    if let Some(max_page) = value("max_page").and_then(|value| value.parse::<u32>().ok()) {
        preferences.max_page = max_page;
    }
    let categories = values("category");
    if !categories.is_empty() {
        preferences.categories = categories
            .into_iter()
            .flat_map(|value| split_list(&value))
            .collect();
    }

    // Engines: the checked boxes are the enabled ones, the difference is stored.
    let enabled_engines: Vec<String> = values("enabled_engine")
        .into_iter()
        .flat_map(|value| split_list(&value))
        .collect();
    if !enabled_engines.is_empty() {
        preferences.disabled_engines = state
            .searcher
            .registry
            .names()
            .into_iter()
            .filter(|name| !enabled_engines.contains(name))
            .collect();
    }

    // Plugins: checked = enabled, everything else is disabled.
    let selected_plugins: Vec<String> = values("enabled_plugin")
        .into_iter()
        .flat_map(|value| split_list(&value))
        .collect();
    if !selected_plugins.is_empty() {
        preferences.disabled_plugins = state
            .searcher
            .plugins
            .infos()
            .into_iter()
            .map(|info| info.id)
            .filter(|id| !selected_plugins.contains(id))
            .collect();
        preferences.enabled_plugins = selected_plugins;
    }

    with_cookies(
        Redirect::to("/preferences").into_response(),
        &preferences.to_cookies(),
    )
}

/// `GET /stats`.
pub async fn stats(State(state): State<AppState>) -> Response {
    let settings = state.settings();
    let engines = state.searcher.engine_snapshots();
    let limiter = state.limiter.stats();
    tracing::debug!(
        enabled = limiter.enabled,
        allowed = limiter.allowed,
        challenged = limiter.challenged,
        blocked = limiter.blocked,
        tracked = limiter.tracked,
        "rate limiter state"
    );
    render(StatsTemplate {
        instance_name: settings.general.instance_name.clone(),
        version: VERSION.to_string(),
        metrics: state.searcher.metrics.snapshot(),
        cache: state.searcher.cache.stats(),
        has_engines: !engines.is_empty(),
        engines,
    })
}

/// `GET /opensearch.xml`.
pub async fn opensearch(State(state): State<AppState>) -> Response {
    super::feeds::opensearch_response(&state)
}

/// `GET /stats/errors`.
pub async fn stats_errors(State(state): State<AppState>) -> Response {
    axum::Json(state.searcher.engine_snapshots()).into_response()
}

/// `GET /metrics`: OpenMetrics / Prometheus text, ported from
/// `searx/openmetrics.py`. Disabled with `general.enable_metrics = false`.
pub async fn metrics(State(state): State<AppState>) -> Response {
    if !state.settings().general.enable_metrics {
        return error_page(&state, StatusCode::NOT_FOUND, "metrics are disabled");
    }
    let body = prometheus_metrics(&state);
    (
        [(
            header::CONTENT_TYPE,
            "application/openmetrics-text; version=1.0.0; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

/// Render the metrics in the Prometheus exposition format. The engine metric
/// names are the upstream ones (`searxng_engines_*`).
fn prometheus_metrics(state: &AppState) -> String {
    let metrics = state.searcher.metrics.snapshot();
    let cache = state.searcher.cache.stats();
    let mut out = String::new();
    let counter = |name: &str, help: &str, out: &mut String| {
        out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} counter\n"));
    };
    counter(
        "searxng_search_requests_total",
        "Number of searches served",
        &mut out,
    );
    out.push_str(&format!(
        "searxng_search_requests_total {}\n",
        metrics.searches
    ));
    counter(
        "searxng_search_results_total",
        "Number of results returned to clients",
        &mut out,
    );
    out.push_str(&format!(
        "searxng_search_results_total {}\n",
        metrics.results
    ));
    out.push_str(
        "# HELP searxng_uptime_seconds Process uptime\n# TYPE searxng_uptime_seconds gauge\n",
    );
    out.push_str(&format!("searxng_uptime_seconds {:.3}\n", metrics.uptime));
    counter("searxng_cache_hit_total", "Number of cache hits", &mut out);
    out.push_str(&format!("searxng_cache_hit_total {}\n", cache.hit));
    counter(
        "searxng_cache_miss_total",
        "Number of cache misses",
        &mut out,
    );
    out.push_str(&format!("searxng_cache_miss_total {}\n", cache.miss));
    out.push_str(
        "# HELP searxng_cache_entries Cached responses\n# TYPE searxng_cache_entries gauge\n",
    );
    out.push_str(&format!("searxng_cache_entries {}\n", cache.entries));

    out.push_str("# HELP searxng_engines_request_count_total Requests sent to an engine\n# TYPE searxng_engines_request_count_total counter\n");
    out.push_str("# HELP searxng_engines_result_count_total Results returned by an engine\n# TYPE searxng_engines_result_count_total counter\n");
    out.push_str("# HELP searxng_engines_error_count_total Errors reported by an engine\n# TYPE searxng_engines_error_count_total counter\n");
    out.push_str("# HELP searxng_engines_response_time_total_seconds Time spent waiting for an engine\n# TYPE searxng_engines_response_time_total_seconds counter\n");
    out.push_str("# HELP searxng_engines_reliability_total Share of successful requests\n# TYPE searxng_engines_reliability_total gauge\n");
    out.push_str("# HELP searxng_engines_suspended Whether the engine is currently suspended\n# TYPE searxng_engines_suspended gauge\n");
    for engine in state.searcher.engine_snapshots() {
        let label = engine.engine.replace('"', "");
        let reliability = if engine.stats.requests == 0 {
            0.0
        } else {
            1.0 - (engine.stats.errors as f64 / engine.stats.requests as f64)
        };
        out.push_str(&format!(
            "searxng_engines_request_count_total{{engine=\"{label}\"}} {}\n",
            engine.stats.requests
        ));
        out.push_str(&format!(
            "searxng_engines_result_count_total{{engine=\"{label}\"}} {}\n",
            engine.stats.results
        ));
        out.push_str(&format!(
            "searxng_engines_error_count_total{{engine=\"{label}\"}} {}\n",
            engine.stats.errors
        ));
        out.push_str(&format!(
            "searxng_engines_response_time_total_seconds{{engine=\"{label}\"}} {:.6}\n",
            engine.stats.total_time
        ));
        out.push_str(&format!(
            "searxng_engines_reliability_total{{engine=\"{label}\"}} {reliability:.6}\n"
        ));
        out.push_str(&format!(
            "searxng_engines_suspended{{engine=\"{label}\"}} {}\n",
            if engine.suspended { 1 } else { 0 }
        ));
    }
    out.push_str("# EOF\n");
    out
}

/// `GET /healthz`.
/// Query parameters of `/research`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ResearchParams {
    pub q: Option<String>,
    pub max_queries: Option<usize>,
    pub max_sources: Option<usize>,
}

/// `GET /research` — deep research: plan sub-queries, search, fetch pages and
/// (when an LLM is configured) synthesise a cited markdown report.
pub async fn research(
    State(state): State<AppState>,
    Query(params): Query<ResearchParams>,
) -> Response {
    let question = params.q.clone().unwrap_or_default();
    if question.trim().is_empty() {
        return error_page(&state, StatusCode::BAD_REQUEST, "missing question");
    }
    let settings = state.settings();
    let max_queries = params
        .max_queries
        .unwrap_or(settings.research.max_queries)
        .clamp(1, 10);
    let max_sources = params
        .max_sources
        .unwrap_or(settings.research.max_sources)
        .clamp(1, 50);
    let report = state
        .searcher
        .research(&question, max_queries, max_sources)
        .await;
    axum::Json(report).into_response()
}

pub async fn healthz() -> Response {
    (StatusCode::OK, "OK").into_response()
}

/// `GET /about`.
pub async fn about(State(state): State<AppState>) -> Response {
    let settings = state.settings();
    render(AboutTemplate {
        instance_name: settings.general.instance_name.clone(),
        version: VERSION.to_string(),
        base_url: state.base_url(),
        engine_count: state.searcher.registry.len(),
        engines: engine_views(&state.searcher),
        plugins: super::plugin_views(
            &state.searcher.plugins,
            &state.settings().plugins.enabled,
            &state.settings().plugins.disabled,
        ),
        settings_path: crate::settings::Settings::path_from_env()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
    })
}

/// `GET /robots.txt`.
pub async fn robots() -> Response {
    let body = "User-agent: *\nDisallow: /stats\nDisallow: /preferences\nDisallow: /config\nDisallow: /metrics\n";
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], body).into_response()
}

/// `GET /favicon.ico`.
pub async fn favicon() -> Response {
    StatusCode::NO_CONTENT.into_response()
}

/// `GET /static/style.css`.
pub async fn style() -> Response {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../../static/style.css"),
    )
        .into_response()
}

/// The fallback route.
pub async fn not_found(State(state): State<AppState>) -> Response {
    error_page(&state, StatusCode::NOT_FOUND, "page not found")
}

/// A 429 (or limiter challenge) response with the limiter's HTML error page.
fn limiter_response(
    state: &AppState,
    retry_after: u64,
    message: &str,
    set_cookie: Option<&str>,
) -> Response {
    let mut response = error_page(state, StatusCode::TOO_MANY_REQUESTS, message);
    if retry_after > 0
        && let Ok(value) = axum::http::HeaderValue::from_str(&retry_after.to_string())
    {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    if let Some(cookie) = set_cookie
        && let Ok(value) = axum::http::HeaderValue::from_str(cookie)
    {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

fn error_page(state: &AppState, status: StatusCode, message: &str) -> Response {
    let body = ErrorTemplate {
        instance_name: state.settings().general.instance_name.clone(),
        version: VERSION.to_string(),
        status: status.as_u16(),
        message: message.to_string(),
    };
    (status, render(body)).into_response()
}

fn preferences_of(headers: &HeaderMap, state: &AppState) -> Preferences {
    Preferences::from_cookies(
        headers
            .get(header::COOKIE)
            .and_then(|value| value.to_str().ok()),
        state.settings(),
    )
}

fn user_info(headers: &HeaderMap) -> UserInfo {
    let ip = headers
        .get("x-forwarded-for")
        .or_else(|| headers.get("x-real-ip"))
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(',')
                .next()
                .unwrap_or_default()
                .trim()
                .to_string()
        })
        .unwrap_or_default();
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    UserInfo {
        ip,
        user_agent,
        headers: Default::default(),
    }
}

fn split_param(value: Option<&str>) -> Option<Vec<String>> {
    value
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect::<Vec<String>>()
        })
        .filter(|values: &Vec<String>| !values.is_empty())
}

/// Parse an `application/x-www-form-urlencoded` body, keeping repeated keys.
pub fn parse_form(body: &str) -> Vec<(String, String)> {
    body.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let mut parts = pair.splitn(2, '=');
            let name = decode_form(parts.next().unwrap_or_default());
            let value = decode_form(parts.next().unwrap_or_default());
            (name, value)
        })
        .collect()
}

fn decode_form(value: &str) -> String {
    percent_encoding::percent_decode_str(&value.replace('+', " "))
        .decode_utf8_lossy()
        .to_string()
}

/// Keep the helper used by the preferences template (it renders lists).
pub fn _join_list(values: &[String]) -> String {
    join_list(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_bodies_keep_repeated_keys() {
        let form = parse_form("action=save&category=general&category=images&q=a+b%26c");
        assert_eq!(
            form,
            vec![
                ("action".to_string(), "save".to_string()),
                ("category".to_string(), "general".to_string()),
                ("category".to_string(), "images".to_string()),
                ("q".to_string(), "a b&c".to_string()),
            ]
        );
    }

    #[test]
    fn comma_separated_parameters_are_split() {
        assert_eq!(
            split_param(Some("general, images")),
            Some(vec!["general".to_string(), "images".to_string()])
        );
        assert_eq!(split_param(Some("")), None);
        assert_eq!(split_param(None), None);
    }
}
