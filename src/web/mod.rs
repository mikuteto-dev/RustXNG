//! HTTP layer, ported from `searx/webapp.py`.
//!
//! Routes match upstream: `/`, `/search` (GET and POST, html/csv/json/rss),
//! `/autocompleter`, `/config`, `/preferences`, `/stats`, `/stats/errors`,
//! `/mcp`, `/about`, `/opensearch.xml`, `/healthz`, `/robots.txt` and `/static/*`.

pub mod favicons;
pub mod feeds;
pub mod handlers;
pub mod image_proxy;
pub mod limiter;
pub mod mcp;
pub mod preferences;

use std::sync::Arc;

use askama::Template;
use axum::Router;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use serde::Serialize;

use crate::aggregator::Searcher;
use crate::results::SearchResult;
use crate::settings::Settings;

/// Shared state of the web application.
#[derive(Clone)]
pub struct AppState {
    pub searcher: Arc<Searcher>,
    /// Overrides the settings base URL (used by the tests).
    pub base_url: Option<String>,
    /// Bot detection / rate limiter (\`server.limiter\`).
    pub limiter: Arc<limiter::Limiter>,
}

impl AppState {
    pub fn new(searcher: Searcher) -> Self {
        let settings = searcher.settings.clone();
        let limiter = Arc::new(limiter::Limiter::from_settings(&settings));
        Self {
            searcher: Arc::new(searcher),
            base_url: None,
            limiter,
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.searcher.settings
    }

    pub fn base_url(&self) -> String {
        self.base_url
            .clone()
            .unwrap_or_else(|| self.settings().base_url())
    }
}

/// The router of the instance.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(handlers::index))
        .route(
            "/search",
            get(handlers::search_get).post(handlers::search_post),
        )
        .route("/autocompleter", get(handlers::autocompleter))
        .route("/config", get(handlers::config))
        .route(
            "/preferences",
            get(handlers::preferences_get).post(handlers::preferences_post),
        )
        .route("/stats", get(handlers::stats))
        .route("/stats/errors", get(handlers::stats_errors))
        .route("/metrics", get(handlers::metrics))
        .route("/image_proxy", get(image_proxy::handler))
        .route("/favicon_proxy", get(favicons::handler))
        .route("/research", get(handlers::research))
        .route("/healthz", get(handlers::healthz))
        .route("/mcp", post(mcp::handler))
        .route("/about", get(handlers::about))
        .route("/opensearch.xml", get(handlers::opensearch))
        .route("/robots.txt", get(handlers::robots))
        .route("/favicon.ico", get(handlers::favicon))
        .route("/static/style.css", get(handlers::style))
        .fallback(handlers::not_found)
        .with_state(state)
}

/// Render a template, turning errors into a 500 response.
pub fn render<T: Template>(template: T) -> Response {
    match template.render() {
        Ok(body) => Html(body).into_response(),
        Err(error) => {
            tracing::error!(%error, "template could not be rendered");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("template error: {error}"),
            )
                .into_response()
        }
    }
}

/// Append `Set-Cookie` headers to a response.
pub fn with_cookies(mut response: Response, cookies: &[(String, String)]) -> Response {
    for (name, value) in cookies {
        if let Ok(header) =
            HeaderValue::from_str(&format!("{name}={value}; Path=/; Max-Age=31536000"))
        {
            response.headers_mut().append(header::SET_COOKIE, header);
        }
    }
    response
}

/// An engine as shown in the UI and the `/config` endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct EngineView {
    pub name: String,
    pub shortcut: String,
    pub categories: Vec<String>,
    pub paging: bool,
    pub time_range_support: bool,
    pub safesearch: bool,
    pub language_support: bool,
    pub enabled: bool,
}

/// A category tab.
#[derive(Debug, Clone, Serialize)]
pub struct CategoryView {
    pub name: String,
    pub selected: bool,
}

/// A result prepared for the templates (no method calls in the templates).
#[derive(Debug, Clone, Serialize)]
pub struct ResultView {
    pub url: String,
    pub title: String,
    pub content: String,
    pub pretty_url: String,
    pub engines: String,
    pub category: String,
    pub template: String,
    pub published_date: String,
    pub img_src: String,
    pub thumbnail: String,
    pub has_thumbnail: bool,
    pub has_image: bool,
    pub img_src_proxy: String,
    pub thumbnail_proxy: String,
    pub metadata: Vec<(String, String)>,
}

impl ResultView {
    pub fn from_result(result: &SearchResult) -> Self {
        Self {
            url: result.url.clone(),
            title: result.title.clone(),
            content: result.content.clone(),
            pretty_url: result
                .pretty_url
                .clone()
                .unwrap_or_else(|| crate::results::pretty_url(&result.url)),
            engines: result.engines.join(", "),
            category: result.category.clone(),
            template: result.template.as_str().to_string(),
            published_date: result.published_date.clone().unwrap_or_default(),
            has_thumbnail: result.thumbnail.is_some(),
            has_image: result.img_src.is_some(),
            img_src_proxy: String::new(),
            thumbnail_proxy: String::new(),
            img_src: result.img_src.clone().unwrap_or_default(),
            thumbnail: result.thumbnail.clone().unwrap_or_default(),
            metadata: result
                .metadata
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        }
    }
}

/// The search form shown on `/`.
#[derive(Debug, Template)]
#[template(path = "index.html")]
pub struct IndexTemplate {
    pub instance_name: String,
    pub version: String,
    pub query: String,
    pub categories: Vec<CategoryView>,
    pub engines: Vec<EngineView>,
    pub method: String,
    pub autocomplete: String,
    pub autocomplete_min: usize,
    pub error: String,
    pub has_error: bool,
    pub engine_count: usize,
    pub time_range: String,
    pub time_ranges: Vec<(String, String)>,
}

/// An infobox prepared for the templates.
#[derive(Debug, Clone, Serialize)]
pub struct InfoboxView {
    pub id: String,
    pub title: String,
    pub content: String,
    pub img_src: String,
    pub url: String,
    pub urls: Vec<(String, String)>,
    pub attributes: Vec<(String, String)>,
    pub has_image: bool,
}

impl InfoboxView {
    pub fn from_infobox(infobox: &crate::results::Infobox) -> Self {
        let img_src = infobox.img_src.clone().unwrap_or_default();
        Self {
            id: infobox.id.clone(),
            title: infobox.title.clone(),
            content: infobox.content.clone(),
            has_image: !img_src.is_empty(),
            img_src,
            url: infobox.url.clone().unwrap_or_default(),
            urls: infobox
                .urls
                .iter()
                .map(|entry| (entry.title.clone(), entry.url.clone()))
                .collect(),
            attributes: infobox
                .attributes
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        }
    }
}

/// The result page.
#[derive(Debug, Template)]
#[template(path = "results.html")]
pub struct ResultsTemplate {
    pub instance_name: String,
    pub version: String,
    pub query: String,
    pub results: Vec<ResultView>,
    pub answers: Vec<String>,
    pub corrections: Vec<String>,
    pub suggestions: Vec<String>,
    pub infoboxes: Vec<InfoboxView>,
    pub unresponsive_engines: Vec<String>,
    pub number_of_results: u64,
    pub pageno: u32,
    pub prev_url: String,
    pub next_url: String,
    pub has_prev: bool,
    pub has_next: bool,
    pub engine_count: usize,
    pub has_answers: bool,
    pub has_corrections: bool,
    pub has_suggestions: bool,
    pub has_infoboxes: bool,
    pub has_unresponsive: bool,
    pub has_engine_data: bool,
    pub has_timings: bool,
    pub has_results: bool,
    pub categories: Vec<CategoryView>,
    pub engines: Vec<EngineView>,
    pub method: String,
    pub autocomplete: String,
    pub autocomplete_min: usize,
    pub time_range: String,
    pub time_ranges: Vec<(String, String)>,
    pub timings: Vec<(String, String)>,
    pub engine_data: Vec<(String, Vec<(String, String)>)>,
}

/// A plugin as shown on the preferences and about pages.
#[derive(Debug, Clone, Serialize)]
pub struct PluginView {
    pub id: String,
    pub name: String,
    pub description: String,
    pub enabled: bool,
}

/// One option of a `<select>` element.
#[derive(Debug, Clone, Serialize)]
pub struct SelectOption {
    pub value: String,
    pub label: String,
    pub selected: bool,
}

/// Build the options of a select element, marking the current value.
pub fn select_options(values: Vec<(String, String)>, current: &str) -> Vec<SelectOption> {
    values
        .into_iter()
        .map(|(value, label)| SelectOption {
            selected: value == current,
            value,
            label,
        })
        .collect()
}

/// The preferences page.
#[derive(Debug, Template)]
#[template(path = "preferences.html")]
pub struct PreferencesTemplate {
    pub instance_name: String,
    pub version: String,
    pub preferences: preferences::Preferences,
    pub categories: Vec<CategoryView>,
    pub engines: Vec<EngineView>,
    pub plugins: Vec<PluginView>,
    pub languages: Vec<SelectOption>,
    pub locales: Vec<SelectOption>,
    pub themes: Vec<SelectOption>,
    pub safesearch_options: Vec<SelectOption>,
    pub autocomplete_options: Vec<SelectOption>,
    pub method_options: Vec<SelectOption>,
    pub formats: Vec<String>,
    pub disabled_engines: String,
    pub enabled_plugins: String,
    pub disabled_plugins: String,
}

/// The about page.
#[derive(Debug, Template)]
#[template(path = "about.html")]
pub struct AboutTemplate {
    pub instance_name: String,
    pub version: String,
    pub base_url: String,
    pub engines: Vec<EngineView>,
    pub plugins: Vec<PluginView>,
    pub settings_path: String,
    pub engine_count: usize,
}

/// The stats page.
#[derive(Debug, Template)]
#[template(path = "stats.html")]
pub struct StatsTemplate {
    pub instance_name: String,
    pub version: String,
    pub metrics: crate::metrics::MetricsSnapshot,
    pub cache: crate::cache::CacheStats,
    pub engines: Vec<crate::engine::state::EngineSnapshot>,
    pub has_engines: bool,
}

/// The error page.
#[derive(Debug, Template)]
#[template(path = "error.html")]
pub struct ErrorTemplate {
    pub instance_name: String,
    pub version: String,
    pub status: u16,
    pub message: String,
}

/// The `time_range` options of the search form.
pub fn time_range_options() -> Vec<(String, String)> {
    vec![
        (String::new(), "Any time".to_string()),
        ("day".to_string(), "Past day".to_string()),
        ("week".to_string(), "Past week".to_string()),
        ("month".to_string(), "Past month".to_string()),
        ("year".to_string(), "Past year".to_string()),
    ]
}

/// Build the engine views of an instance.
pub fn engine_views(searcher: &Searcher) -> Vec<EngineView> {
    searcher
        .registry
        .iter()
        .map(|engine| EngineView {
            name: engine.name().to_string(),
            shortcut: engine.shortcut().to_string(),
            categories: engine.categories().to_vec(),
            paging: engine.paging(),
            time_range_support: engine.time_range_support(),
            safesearch: engine.safesearch_support(),
            language_support: engine.language_support(),
            enabled: true,
        })
        .collect()
}

/// Engine views marking the engines disabled in the preferences.
pub fn engine_views_for(searcher: &Searcher, disabled: &[String]) -> Vec<EngineView> {
    let mut views = engine_views(searcher);
    for view in &mut views {
        view.enabled = !disabled.contains(&view.name);
    }
    views
}

/// Plugin views marking the plugins the user selected.
pub fn plugin_views(
    plugins: &crate::plugins::PluginManager,
    enabled: &[String],
    disabled: &[String],
) -> Vec<PluginView> {
    plugins
        .infos()
        .into_iter()
        .map(|info| {
            let selected = enabled.iter().any(|value| value == &info.id)
                || (info.default_enabled && !disabled.iter().any(|value| value == &info.id));
            PluginView {
                id: info.id,
                name: info.name,
                description: info.description,
                enabled: selected,
            }
        })
        .collect()
}

/// The comma separated list of engine names used by the preferences form.
pub fn engine_list(searcher: &Searcher, disabled: &[String]) -> String {
    searcher
        .registry
        .iter()
        .map(|engine| engine.name().to_string())
        .filter(|name| !disabled.contains(name))
        .collect::<Vec<_>>()
        .join(",")
}

/// Build the category tabs, marking the selected ones.
pub fn category_views(searcher: &Searcher, selected: &[String]) -> Vec<CategoryView> {
    let mut names: Vec<String> = searcher.settings.categories_as_tabs.clone();
    if names.is_empty() {
        names = crate::categories::TABS
            .iter()
            .map(|category| (*category).to_string())
            .collect();
    }
    names
        .into_iter()
        .map(|name| CategoryView {
            selected: selected.iter().any(|value| {
                crate::categories::normalize(value) == crate::categories::normalize(&name)
            }),
            name,
        })
        .collect()
}
