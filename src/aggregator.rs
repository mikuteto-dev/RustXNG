//! Search orchestration, ported from `searx/search/processors/online.py` and
//! `searx/search/__init__.py`: engine selection, concurrent execution,
//! per engine timeouts, suspensions, caching and result merging.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::cache::Cache;
use crate::engine::state::{EngineSnapshot, EngineStates};
use crate::engine::{EngineHandle, EngineItem, EngineRegistry, FailureKind};
use crate::metrics::Metrics;
use crate::network::Network;
use crate::plugins::{PluginContext, PluginManager, UserInfo};
use crate::query::{Query, TimeRange};
use crate::results::{Infobox, ResultContainer, SearchResult, Timing, UnresponsiveEngine};
use crate::settings::Settings;

/// Everything the searcher needs to know about one search request.
#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub query: Query,
    pub pageno: u32,
    pub categories: Vec<String>,
    pub engines: Vec<String>,
    pub disabled_engines: Vec<String>,
    pub time_range: Option<TimeRange>,
    pub safesearch: u8,
    pub language: String,
    pub timeout_limit: Option<f64>,
    pub enabled_plugins: Vec<String>,
    pub disabled_plugins: Vec<String>,
    pub locale: String,
    pub user: UserInfo,
    /// Keep at most this many results (agent-facing extension).
    pub max_results: Option<usize>,
    /// Keep only results whose host matches one of these domains.
    pub include_domains: Vec<String>,
    /// Drop results whose host matches one of these domains.
    pub exclude_domains: Vec<String>,
    /// Fetch the page text for the top results and return it as raw_content.
    pub include_raw_content: bool,
    /// Re-rank results in-process with EmbeddingGemma 2.
    pub semantic: bool,
    /// How the selected engines are started.
    pub strategy: crate::settings::SearchStrategy,
    /// Set once the site-scoped second pass already ran, so it cannot recurse.
    pub site_retry_done: bool,
}

impl SearchRequest {
    /// A request with the defaults for a query, used by the API and tests.
    pub fn new(query: Query) -> Self {
        Self {
            pageno: 1,
            categories: query.selected_categories(),
            engines: Vec::new(),
            disabled_engines: query.disabled_engines.clone(),
            time_range: None,
            safesearch: query.safesearch,
            language: query.engine_language(),
            timeout_limit: query.timeout_limit,
            enabled_plugins: Vec::new(),
            disabled_plugins: Vec::new(),
            locale: query.locale.clone(),
            user: UserInfo::default(),
            max_results: None,
            include_domains: Vec::new(),
            exclude_domains: Vec::new(),
            include_raw_content: false,
            semantic: false,
            strategy: crate::settings::SearchStrategy::Parallel,
            site_retry_done: false,
            query,
        }
    }
}

/// The result of a search, mirroring the fields of upstream's JSON response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub query: String,
    pub search_query: String,
    pub number_of_results: u64,
    pub results: Vec<SearchResult>,
    /// Upstream's JSON API emits an array of `{"answer": ...}` objects.
    #[serde(serialize_with = "serialize_answers")]
    pub answers: Vec<String>,
    pub corrections: Vec<String>,
    pub infoboxes: Vec<Infobox>,
    pub suggestions: Vec<String>,
    /// Upstream's JSON API emits `[engine, translated error]` pairs.
    #[serde(serialize_with = "serialize_unresponsive")]
    pub unresponsive_engines: Vec<UnresponsiveEngine>,
    pub timings: Vec<Timing>,
    pub redirect_url: Option<String>,
    pub engine_data: BTreeMap<String, BTreeMap<String, String>>,
    pub engines: Vec<String>,
    pub pageno: u32,
    pub paging: bool,
    /// Set when the request itself could not be served (no engine, ...).
    pub error: Option<String>,
}

impl SearchResponse {
    pub fn new(query: &Query) -> Self {
        Self {
            query: query.full_query(),
            search_query: query.query_text(),
            number_of_results: 0,
            results: Vec::new(),
            answers: Vec::new(),
            corrections: Vec::new(),
            infoboxes: Vec::new(),
            suggestions: Vec::new(),
            unresponsive_engines: Vec::new(),
            timings: Vec::new(),
            redirect_url: None,
            engine_data: BTreeMap::new(),
            engines: Vec::new(),
            pageno: 1,
            paging: false,
            error: None,
        }
    }
}

/// The outcome of one engine run.
struct EngineOutcome {
    engine: String,
    weight: f64,
    items: Result<Vec<EngineItem>, FailureKind>,
    elapsed: Duration,
    suspended: bool,
}

/// Runs searches against the configured engines.
pub struct Searcher {
    pub settings: Arc<Settings>,
    pub registry: Arc<EngineRegistry>,
    pub network: Arc<Network>,
    pub cache: Arc<Cache>,
    pub states: Arc<EngineStates>,
    pub metrics: Arc<Metrics>,
    pub plugins: Arc<PluginManager>,
    semantic: Arc<tokio::sync::Mutex<Option<Arc<crate::semantic::EmbeddingGemma2>>>>,
    /// Our own BM25 index, when one is configured and opened.
    pub local_index: Option<Arc<crate::localindex::LocalIndex>>,
}

/// Open the configured local index, if any.
fn open_local_index(settings: &Settings) -> Option<Arc<crate::localindex::LocalIndex>> {
    let path = settings.search.local_index_path.trim();
    if path.is_empty() {
        return None;
    }
    match crate::localindex::LocalIndex::open(std::path::Path::new(path)) {
        Ok(index) => {
            tracing::info!(
                path,
                documents = index.document_count(),
                "local index opened"
            );
            Some(Arc::new(index))
        }
        Err(error) => {
            tracing::warn!(path, %error, "local index could not be opened");
            None
        }
    }
}

impl Searcher {
    pub fn new(settings: Settings) -> Result<Self, String> {
        let registry = EngineRegistry::from_settings(&settings)?;
        let network = Network::from_settings(&settings).map_err(|error| error.to_string())?;
        let cache = match settings.cache.backend.as_str() {
            "valkey" | "redis" => Cache::valkey(
                &settings.cache.url,
                Duration::from_secs(settings.cache.ttl.max(1)),
            ),
            _ => Cache::memory(
                Duration::from_secs(settings.cache.ttl.max(1)),
                settings.cache.max_bytes.max(1024),
            ),
        };
        Ok(Self {
            local_index: open_local_index(&settings),
            plugins: Arc::new(PluginManager::from_settings(&settings)),
            settings: Arc::new(settings),
            registry: Arc::new(registry),
            network: Arc::new(network),
            cache: Arc::new(cache),
            states: Arc::new(EngineStates::new()),
            metrics: Arc::new(Metrics::new()),
            semantic: Arc::new(tokio::sync::Mutex::new(None)),
        })
    }

    /// Engines that answer a request, in registry order.
    pub fn select_engines(&self, request: &SearchRequest) -> Vec<EngineHandle> {
        let explicit: Vec<String> = if !request.engines.is_empty() {
            request.engines.clone()
        } else {
            request.query.engine_ref_names()
        };
        let wanted_categories: Vec<String> = if !request.categories.is_empty() {
            request.categories.clone()
        } else {
            vec![crate::categories::GENERAL.to_string()]
        };

        // SearXNG ignores unknown engine names: fall back to the default set
        // when nothing matched, instead of failing the request.
        let run = |explicit: &[String]| {
            let mut selected = Vec::new();
            for engine in self.registry.iter() {
                let name = engine.name().to_string();
                if request.disabled_engines.contains(&name) {
                    continue;
                }
                if !explicit.is_empty() {
                    if explicit
                        .iter()
                        .any(|wanted| wanted.eq_ignore_ascii_case(&name))
                    {
                        selected.push(engine.clone());
                    }
                    continue;
                }
                let matches = engine.categories().iter().any(|category| {
                    let normalised = crate::categories::normalize(category);
                    wanted_categories
                        .iter()
                        .any(|wanted| crate::categories::normalize(wanted) == normalised)
                });
                if matches {
                    selected.push(engine.clone());
                }
            }
            selected
        };
        let selected = run(&explicit);
        if selected.is_empty() && !explicit.is_empty() {
            return run(&[]);
        }
        selected
    }

    /// Run a search: this is the entry point used by the web layer.
    pub async fn search(&self, mut request: SearchRequest) -> SearchResponse {
        Metrics::incr(&self.metrics.searches, 1);
        let context = PluginContext {
            search_query: request.query.query_text(),
            locale: if request.locale.is_empty() {
                self.settings.search.default_lang.clone()
            } else {
                request.locale.clone()
            },
            base_url: self.settings.base_url(),
            user: request.user.clone(),
        };
        self.plugins.pre_search(
            &mut request.query,
            &context,
            &request.enabled_plugins,
            &request.disabled_plugins,
        );

        let mut response = SearchResponse::new(&request.query);
        response.pageno = request.pageno;

        // External bang: hand the query over to another service.
        if let Some(bang) = request.query.external_bang.clone() {
            if let Some(definition) = crate::external_bangs::lookup(&bang) {
                response.redirect_url = Some(crate::external_bangs::expand_url(
                    definition.url,
                    &request.query.query_text(),
                ));
                response.engines = Vec::new();
                return response;
            }
        }
        let engines = self.select_engines(&request);
        if engines.is_empty() {
            response.error = Some("no engine selected for this request".to_string());
            return response;
        }

        // 全 engine の完了を待つ（join_all）。
        //
        // 以前はここで timeout_limit を engine ごとに一律適用していたため、
        // **速い engine が締め切りに巻き込まれて結果を捨てていた**（実測: 45ms で
        // 終わる engine が 102ms の締め切りで Timeout 扱いになり 0 件）。
        // 締め切りは「検索全体」に対してかけ、完了済みの結果は必ず活かす。
        let deadline = Instant::now()
            + Duration::from_secs_f64(
                request
                    .timeout_limit
                    .unwrap_or(self.settings.search.timeout_limit)
                    .max(0.1),
            );
        let outcomes = match request.strategy {
            crate::settings::SearchStrategy::Parallel => {
                futures::future::join_all(
                    engines
                        .iter()
                        .map(|engine| self.run_engine(engine, &request, deadline)),
                )
                .await
            }
            crate::settings::SearchStrategy::Staged => {
                self.run_staged(&engines, &request, deadline).await
            }
        };

        // Remember which engines answered before the outcomes are consumed;
        // the deep-page pass only asks those again.
        let healthy: Vec<String> = outcomes
            .iter()
            .filter(|outcome| outcome.items.is_ok())
            .map(|outcome| outcome.engine.clone())
            .collect();

        let mut container = ResultContainer::new();
        for outcome in outcomes {
            response.engines.push(outcome.engine.clone());
            match outcome.items {
                Ok(items) => container.extend(&outcome.engine, outcome.weight, items),
                Err(kind) => {
                    container.add_unresponsive_engine(
                        &outcome.engine,
                        kind.error_type(),
                        outcome.suspended,
                    );
                }
            }
            container.add_timing(&outcome.engine, outcome.elapsed.as_secs_f64(), 0.0);
        }

        // Extra result pages: coverage, not ranking, is what limits recall for
        // documentation questions, so page 2 is worth fetching when asked for.
        let deep_pages = self.settings.search.deep_pages;
        if deep_pages > 0 && request.engines.len() != 1 && Instant::now() < deadline {
            let paging: Vec<EngineHandle> = engines
                .iter()
                .filter(|engine| engine.paging() && healthy.contains(&engine.name().to_string()))
                .cloned()
                .collect();
            for extra in 1..=deep_pages {
                if paging.is_empty() || Instant::now() >= deadline {
                    break;
                }
                let mut deeper = request.clone();
                deeper.pageno = request.pageno + extra;
                let page_outcomes = futures::future::join_all(
                    paging
                        .iter()
                        .map(|engine| self.run_engine(engine, &deeper, deadline)),
                )
                .await;
                for outcome in page_outcomes {
                    if let Ok(items) = outcome.items {
                        container.extend_from_page(&outcome.engine, outcome.weight, items, extra);
                    }
                }
            }
        }

        let query_terms = crate::results::query_terms(&request.query.query_text());
        // Relevance re-ranking only applies when the response merges several
        // engines (the general search). A single pinned engine keeps the
        // engine's own order, which preserves exact SearXNG overlap@10.
        let relevance_weight = if request.engines.len() == 1 {
            0.0
        } else {
            self.settings.search.relevance_weight
        };
        // Canonical-site boost: naming a site means "that site's own page".
        let query_text = request.query.query_text();
        let authority_weight = if request.engines.len() == 1 {
            0.0
        } else {
            self.settings.search.authority_weight
        };
        // The local index is just another source of pages.
        if self.settings.search.local_index_merge {
            if let Some(index) = self.local_index.as_ref() {
                if request.engines.len() != 1 {
                    let text = request.query.query_text();
                    if let Ok(hits) = index.search(&text, 20) {
                        let items: Vec<EngineItem> = hits
                            .into_iter()
                            .map(|hit| {
                                let mut result = SearchResult::new(hit.url, hit.title);
                                result.score = f64::from(hit.score);
                                EngineItem::Result(result)
                            })
                            .collect();
                        container.extend("local_index", 1.0, items);
                    }
                }
            }
        }

        container.close(
            |engine| self.registry.weight_of(engine),
            &query_terms,
            relevance_weight,
            &query_text,
            authority_weight,
        );

        let mut results = container.ordered().to_vec();
        for result in &mut results {
            self.plugins.on_result(
                result,
                &context,
                &request.enabled_plugins,
                &request.disabled_plugins,
            );
        }

        // Semantic re-ranking for merged multi-engine results. EmbeddingGemma 2
        // is opt-in per request; the external endpoint remains configuration-driven.
        if request.semantic
            || (request.engines.len() != 1 && !self.settings.search.embedding_url.is_empty())
        {
            results = self
                .rerank_by_embedding(&request.query, results, request.semantic)
                .await;
        }

        // Site-scoped second pass: when the query names a known site but that
        // site barely shows up, search within it explicitly. Metasearch engines
        // answer "python dict get method" with a tutorial page; asking for a
        // site-scoped query surfaces the reference page itself.
        if !request.site_retry_done {
            results = self.site_scoped_retry(&request, results).await;
        }

        crate::results::filter_results(
            &mut results,
            &request.include_domains,
            &request.exclude_domains,
            request.max_results,
        );
        if request.include_raw_content {
            self.attach_raw_content(&mut results).await;
        }

        response.number_of_results = container.number_of_results();
        response.paging = container.paging;
        response.answers = container.answers.clone();
        response.corrections = container.corrections.iter().cloned().collect();
        response.infoboxes = container.infoboxes.clone();
        response.suggestions = container.suggestions.iter().cloned().collect();
        response.unresponsive_engines = container.unresponsive_engines.clone();
        response.timings = container.timings.clone();
        response.engine_data = container.engine_data.clone();
        response.results = results;

        if request.query.redirect_to_first_result {
            if let Some(first) = response.results.first() {
                response.redirect_url = Some(first.url.clone());
            }
        }

        self.plugins.post_search(
            &mut response,
            &context,
            &request.enabled_plugins,
            &request.disabled_plugins,
        );
        // Upstream answerers (statistics, random, weather) answer in front of
        // the results; they run after the plugins like upstream's answerers.
        let answer_context = crate::answerers::AnswerContext {
            locale: context.locale.clone(),
            base_url: context.base_url.clone(),
            network: Some(self.network.clone()),
        };
        let answers = crate::answerers::ask(
            &crate::answerers::all(),
            &response.search_query,
            &answer_context,
        )
        .await;
        response.answers.extend(answers);
        Metrics::incr(&self.metrics.results, response.results.len() as u64);
        response
    }

    async fn run_engine(
        &self,
        engine: &EngineHandle,
        request: &SearchRequest,
        deadline: Instant,
    ) -> EngineOutcome {
        let name = engine.name().to_string();
        let weight = engine.weight();

        if let Some((remaining, reason)) = self.states.suspension(&name) {
            let kind = failure_kind_from_error_type(reason.as_deref().unwrap_or("suspended"));
            tracing::debug!(engine = %name, remaining = ?remaining, "engine suspended, skipping");
            return EngineOutcome {
                engine: name,
                weight,
                items: Err(kind),
                elapsed: Duration::ZERO,
                suspended: true,
            };
        }

        let started = Instant::now();
        // engine ごとの timeout は残り時間で上限を切る。締め切りを過ぎていれば走らせない。
        let Some(timeout) = engine_budget(engine.timeout(), deadline, Instant::now()) else {
            return EngineOutcome {
                engine: name,
                weight,
                items: Err(FailureKind::Timeout),
                elapsed: Duration::ZERO,
                suspended: false,
            };
        };

        Metrics::incr(&self.metrics.engine_requests, 1);
        let items = self.run_engine_inner(engine, request, timeout).await;
        let elapsed = started.elapsed();

        match &items {
            Ok(items) => {
                self.states
                    .record_success(&name, items.len(), elapsed, &self.settings.search)
            }
            Err(kind) => {
                Metrics::incr(&self.metrics.engine_errors, 1);
                if *kind == FailureKind::Timeout {
                    Metrics::incr(&self.metrics.engine_timeouts, 1);
                }
                self.states
                    .record_failure(&name, *kind, &self.settings.search);
            }
        }

        EngineOutcome {
            engine: name,
            weight,
            items,
            elapsed,
            suspended: false,
        }
    }

    async fn run_engine_inner(
        &self,
        engine: &EngineHandle,
        request: &SearchRequest,
        timeout: Duration,
    ) -> Result<Vec<EngineItem>, FailureKind> {
        // Database backed engines do their work on a blocking thread; the HTTP
        // path below only applies to network engines.
        if engine.is_offline() {
            let handle = Arc::clone(engine);
            let query = request.query.clone();
            let pageno = request.pageno;
            let joined = tokio::time::timeout(
                timeout,
                tokio::task::spawn_blocking(move || handle.search_offline(&query, pageno)),
            )
            .await;
            return match joined {
                Ok(Ok(Ok(items))) => check_quality(engine.name(), &request.query, items),
                Ok(Ok(Err(error))) => Err(FailureKind::from(&error)),
                Ok(Err(_)) => Err(FailureKind::Other),
                Err(_) => Err(FailureKind::Timeout),
            };
        }
        let engine_request = engine
            .build_request(&request.query, request.pageno)
            .map_err(|error| FailureKind::from(&error))?;
        let response = self
            .network
            .fetch(engine_request, timeout, Some(&self.cache), engine.proxy())
            .await
            .map_err(|error| error.failure_kind())?;
        let items = engine
            .parse(&response, &request.query)
            .map_err(|error| FailureKind::from(&error))?;
        check_quality(engine.name(), &request.query, items)
    }

    /// Start engines tier by tier and stop once enough results were collected.
    ///
    /// SearXNG issue #6825: querying every engine at once is what makes many
    /// engines suspend together. The staged strategy starts a smaller tier,
    /// checks whether the results are already enough and only then continues.
    async fn run_staged(
        &self,
        engines: &[EngineHandle],
        request: &SearchRequest,
        deadline: Instant,
    ) -> Vec<EngineOutcome> {
        let tier = self.settings.search.staged_tier.max(1);
        let target = self.settings.search.staged_min_results.max(1);
        let ordered = self.order_engines_by_health(engines);
        let mut outcomes = Vec::new();
        let mut collected = 0usize;
        for chunk in ordered.chunks(tier) {
            if Instant::now() >= deadline {
                break;
            }
            let tier_outcomes = futures::future::join_all(
                chunk
                    .iter()
                    .map(|engine| self.run_engine(engine, request, deadline)),
            )
            .await;
            for outcome in &tier_outcomes {
                if let Ok(items) = &outcome.items {
                    collected += items.len();
                }
            }
            outcomes.extend(tier_outcomes);
            if collected >= target {
                break;
            }
        }
        outcomes
    }

    /// Order engines for the staged strategy.
    ///
    /// Proven engines (requests with results) go first, fastest first; engines
    /// that have not been tried yet keep their registry order; engines that
    /// failed or are suspended go last.
    fn order_engines_by_health(&self, engines: &[EngineHandle]) -> Vec<EngineHandle> {
        use std::collections::HashMap;
        let mut ranks: HashMap<String, (u8, u64)> = HashMap::new();
        for snapshot in self.states.snapshot() {
            let stats = &snapshot.stats;
            let rank = if snapshot.suspended {
                3
            } else if stats.requests == 0 {
                1
            } else if stats.results > 0 {
                0
            } else {
                2
            };
            let millis = if stats.requests == 0 {
                0
            } else {
                (stats.average_time() * 1000.0) as u64
            };
            ranks.insert(snapshot.engine, (rank, millis));
        }
        let mut ordered: Vec<EngineHandle> = engines.to_vec();
        ordered.sort_by_key(|engine| ranks.get(engine.name()).copied().unwrap_or((1, 0)));
        ordered
    }

    /// Fetch readable page text for the first few results (raw content).
    /// Run a site-scoped search when the site named in the query is missing
    /// from the first pass, and splice its best pages into the results.
    async fn site_scoped_retry(
        &self,
        request: &SearchRequest,
        mut results: Vec<SearchResult>,
    ) -> Vec<SearchResult> {
        if !self.settings.search.site_scoped_retry || request.engines.len() == 1 {
            return results;
        }
        let text = request.query.query_text();
        let Some(host) = crate::results::canonical_host_for_query(&text) else {
            return results;
        };
        // Only the top of the list matters: a docs page buried at rank 40 does
        // not answer the query, even if the site is technically present.
        let present = results
            .iter()
            .take(3)
            .filter(|result| {
                crate::results::host_of(&result.url)
                    .is_some_and(|h| h == host || h.ends_with(&format!(".{host}")))
            })
            .count();
        if present >= 2 {
            return results;
        }

        let scoped = format!("site:{host} {text}");
        let mut second = request.clone();
        second.query =
            crate::query::Query::parse(&scoped, &self.registry, &request.disabled_engines);
        second.site_retry_done = true;
        second.timeout_limit = Some(
            request
                .timeout_limit
                .unwrap_or(self.settings.search.timeout_limit)
                .min(2.0),
        );
        second.include_raw_content = false;
        second.max_results = None;
        let response = Box::pin(self.search(second)).await;
        tracing::debug!(
            host,
            present,
            second_results = response.results.len(),
            "site-scoped second pass ran"
        );

        let reference = results.first().map(|result| result.score).unwrap_or(1.0);
        let terms = crate::results::query_terms(&text);
        // Pick the pages of this site that best match the original query, not
        // merely the ones the scoped pass happened to rank first.
        let mut candidates: Vec<SearchResult> = response
            .results
            .into_iter()
            .filter(|candidate| {
                crate::results::host_of(&candidate.url)
                    .is_some_and(|h| h == host || h.ends_with(&format!(".{host}")))
            })
            .collect();
        candidates.sort_by(|a, b| {
            crate::results::relevance_score(b, &terms)
                .partial_cmp(&crate::results::relevance_score(a, &terms))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut added = 0usize;
        let mut by_key: std::collections::HashMap<String, usize> = results
            .iter()
            .enumerate()
            .map(|(index, result)| (crate::results::dedup_key(&result.url), index))
            .collect();
        for candidate in candidates {
            if added >= 3 {
                break;
            }
            let key = crate::results::dedup_key(&candidate.url);
            if let Some(index) = by_key.get(&key).copied() {
                results[index].score = results[index].score.max(reference * 0.95);
                continue;
            }
            let mut promoted = candidate;
            promoted.score = reference * (0.95 - 0.10 * added as f64);
            by_key.insert(key, results.len());
            results.push(promoted);
            added += 1;
        }
        if added > 0 {
            tracing::debug!(host, added, "site-scoped second pass added pages");
            results.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
        results
    }

    /// Re-rank the top results by cosine similarity between the query and each
    /// result's title + snippet, using an OpenAI-compatible embeddings endpoint.
    ///
    /// Returns the input unchanged when the endpoint fails (degrades to the
    /// position + relevance ranking).
    async fn rerank_by_embedding(
        &self,
        query: &Query,
        mut results: Vec<SearchResult>,
        local: bool,
    ) -> Vec<SearchResult> {
        let settings = &self.settings.search;
        let topk = results.len().min(settings.embedding_topk);
        if topk < 2 {
            return results;
        }
        let mut texts = vec![if local {
            format!("task: search result | query: {}", query.query_text())
        } else {
            query.query_text()
        }];
        for result in &results[..topk] {
            let snippet: String = result.content.chars().take(240).collect();
            texts.push(if local {
                format!("title: {} | text: {snippet}", result.title)
            } else {
                format!("{} {snippet}", result.title)
            });
        }
        let expected = texts.len();
        let vectors = match if local {
            self.embed_local(texts).await
        } else {
            self.embed(&texts).await
        } {
            Ok(vectors) if vectors.len() == expected => vectors,
            Ok(vectors) => {
                tracing::warn!(
                    expected,
                    actual = vectors.len(),
                    "embedding re-ranking skipped"
                );
                return results;
            }
            Err(error) => {
                tracing::warn!(%error, "embedding re-ranking skipped");
                return results;
            }
        };
        let query_vec = &vectors[0];
        for (index, result) in results.iter_mut().take(topk).enumerate() {
            let cosine = dot(query_vec, &vectors[index + 1]);
            result.score += settings.embedding_weight * cosine;
        }
        results[..topk].sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results
    }

    async fn embed_local(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
        let model = {
            let mut slot = self.semantic.lock().await;
            if slot.is_none() {
                tracing::info!("loading EmbeddingGemma 2 (first use downloads about 550 MB)");
                let model = tokio::task::spawn_blocking(crate::semantic::EmbeddingGemma2::load)
                    .await
                    .map_err(|error| error.to_string())??;
                *slot = Some(Arc::new(model));
            }
            slot.as_ref().expect("semantic model was loaded").clone()
        };
        tokio::task::spawn_blocking(move || model.embed(&texts))
            .await
            .map_err(|error| error.to_string())?
    }

    /// Ask the embeddings endpoint for one vector per input text.
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        let base = self.settings.search.embedding_url.trim_end_matches('/');
        let url = if base.ends_with("/embeddings") {
            base.to_string()
        } else if base.ends_with("/v1") {
            format!("{base}/embeddings")
        } else {
            format!("{base}/v1/embeddings")
        };
        let body = serde_json::json!({
            "model": self.settings.search.embedding_model,
            "input": texts,
        });
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|error| error.to_string())?;
        let response = client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status();
        let payload: serde_json::Value =
            response.json().await.map_err(|error| error.to_string())?;
        if !status.is_success() {
            return Err(format!("embeddings http {}", status.as_u16()));
        }
        let mut data = payload
            .get("data")
            .and_then(|value| value.as_array())
            .cloned()
            .ok_or_else(|| "embeddings: missing data".to_string())?;
        data.sort_by_key(|item| item.get("index").and_then(|v| v.as_u64()).unwrap_or(0));
        data.iter()
            .map(|item| {
                item.get("embedding")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_f64())
                            .map(|v| v as f32)
                            .collect()
                    })
                    .ok_or_else(|| "embeddings: missing vector".to_string())
            })
            .collect()
    }

    async fn attach_raw_content(&self, results: &mut [SearchResult]) {
        let limit = self.settings.search.raw_content_limit.max(1);
        let count = results.len().min(limit);
        if count == 0 {
            return;
        }
        let texts = futures::future::join_all(
            results
                .iter()
                .take(count)
                .map(|result| self.page_text(&result.url)),
        )
        .await;
        for (result, text) in results.iter_mut().take(count).zip(texts) {
            result.raw_content = text;
        }
    }

    /// The readable text of one page, `None` when it cannot be fetched.
    async fn page_text(&self, url: &str) -> Option<String> {
        // Result URLs come from third-party engines, so treat them as untrusted:
        // an engine (or a poisoned result) must not make us fetch localhost or
        // the cloud metadata endpoint on its behalf.
        if !crate::web::image_proxy::is_public_url(url) {
            tracing::debug!(url, "refusing to fetch a non-public page");
            return None;
        }
        let request = crate::engine::EngineRequest::get(url)
            .header("Accept", "text/html,application/xhtml+xml;q=0.9,*/*;q=0.8");
        let response = self
            .network
            .fetch(request, Duration::from_secs(4), Some(&self.cache), None)
            .await
            .ok()?;
        let html = response.html().ok()?;
        let text =
            crate::content::main_text_limited(&html, self.settings.search.raw_content_chars.max(1));
        if text.is_empty() { None } else { Some(text) }
    }

    /// Engine statistics for `/stats`.
    pub fn engine_snapshots(&self) -> Vec<EngineSnapshot> {
        self.states.snapshot()
    }
}

/// Reject a parsed response that looks fabricated or unrelated.
///
/// Upstream SearXNG issue #6358: a blocked upstream can answer with HTTP 200
/// and placeholder content that still parses. Dropping such an engine is
/// better than poisoning the merged result set.
fn check_quality(
    engine: &str,
    query: &crate::query::Query,
    items: Vec<crate::engine::EngineItem>,
) -> Result<Vec<crate::engine::EngineItem>, FailureKind> {
    if let Some(suspicion) = crate::quality::inspect(query, &items) {
        tracing::warn!(
            %engine,
            reason = suspicion.reason(),
            "discarding a suspicious engine response"
        );
        return Err(FailureKind::Suspicious);
    }
    Ok(items)
}

/// `[{"answer": "..."}]`, the shape upstream uses for answers.
fn serialize_answers<S>(answers: &[String], serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::Serialize;
    #[derive(Serialize)]
    struct Answer<'a> {
        answer: &'a str,
    }
    answers
        .iter()
        .map(|answer| Answer { answer })
        .collect::<Vec<_>>()
        .serialize(serializer)
}

/// `[[engine, "Translated error"]]`, the shape upstream uses.
fn serialize_unresponsive<S>(
    engines: &[UnresponsiveEngine],
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::Serialize;
    let mut pairs: Vec<(String, String)> = engines
        .iter()
        .map(|engine| {
            let mut text = match engine.error_type.as_str() {
                "timeout" => "Timeout".to_string(),
                "access denied" => "Access denied".to_string(),
                "captcha" => "CAPTCHA".to_string(),
                "too many requests" => "Too many requests".to_string(),
                "http error" => "HTTP error".to_string(),
                "parsing error" => "Parsing error".to_string(),
                "suspended" => "Suspended".to_string(),
                _ => "Unexpected error".to_string(),
            };
            if engine.suspended && text != "Suspended" {
                text = format!("Suspended: {text}");
            }
            (engine.engine.clone(), text)
        })
        .collect();
    pairs.sort();
    pairs.serialize(serializer)
}

/// Cosine similarity of two equally sized vectors.
fn dot(a: &[f32], b: &[f32]) -> f64 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut norm_a = 0.0f64;
    let mut norm_b = 0.0f64;
    for (x, y) in a.iter().zip(b) {
        let x = *x as f64;
        let y = *y as f64;
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a.sqrt() * norm_b.sqrt())
    }
}

fn failure_kind_from_error_type(error_type: &str) -> FailureKind {
    match error_type {
        "timeout" => FailureKind::Timeout,
        "access denied" => FailureKind::AccessDenied,
        "captcha" => FailureKind::Captcha,
        "too many requests" => FailureKind::TooManyRequests,
        "http error" => FailureKind::Http,
        "parsing error" => FailureKind::Parse,
        "suspicious response" => FailureKind::Suspicious,
        _ => FailureKind::Other,
    }
}

/// How long an engine may run, given the whole-search deadline.
///
/// The engine's own timeout is the ceiling; the search deadline may cut it short.
/// Returns `None` when the deadline has already passed (do not run at all).
fn engine_budget(engine_timeout: Duration, deadline: Instant, now: Instant) -> Option<Duration> {
    let remaining = deadline.saturating_duration_since(now);
    if remaining.is_zero() {
        return None;
    }
    Some(engine_timeout.min(remaining))
}

#[cfg(test)]
mod orchestration_tests {
    use super::*;
    use crate::engine::{Engine, EngineError, EngineMeta, EngineRequest, EngineResponse};

    /// An engine that never touches the network: it either fails while
    /// building the request or returns synthetic results.
    struct FakeEngine {
        meta: EngineMeta,
        results: usize,
        build_error: bool,
    }

    impl FakeEngine {
        fn build(
            name: &str,
            categories: &[&str],
            results: usize,
            build_error: bool,
        ) -> EngineHandle {
            let config = crate::settings::EngineConfig::new(name, "fake", name, categories);
            let mut meta = EngineMeta::from_config(
                &config,
                crate::engine::EngineDefaults {
                    categories: &["general"],
                    shortcut: "fake",
                    ..Default::default()
                },
            );
            meta.categories = categories.iter().map(|c| (*c).to_string()).collect();
            std::sync::Arc::new(Self {
                meta,
                results,
                build_error,
            })
        }
    }

    impl Engine for FakeEngine {
        fn meta(&self) -> &EngineMeta {
            &self.meta
        }
        fn build_request(&self, query: &Query, _pageno: u32) -> Result<EngineRequest, EngineError> {
            if self.build_error {
                return Err(EngineError::Message("boom".into()));
            }
            Ok(EngineRequest::get(format!(
                "https://fake.invalid/?q={}",
                crate::engine::util::encode(&query.query_text())
            )))
        }
        fn parse(
            &self,
            _response: &EngineResponse,
            _query: &Query,
        ) -> Result<Vec<EngineItem>, EngineError> {
            Ok((0..self.results)
                .map(|index| {
                    EngineItem::Result(SearchResult::new(
                        format!("https://example.com/{}/{index}", self.meta.name),
                        format!("{} result {index}", self.meta.name),
                    ))
                })
                .collect())
        }
    }

    fn searcher(engines: Vec<EngineHandle>) -> Searcher {
        let settings = Settings::defaults();
        let mut registry = EngineRegistry::default();
        for handle in engines {
            registry.push(handle);
        }
        let network = Network::from_settings(&settings).unwrap();
        Searcher {
            local_index: None,
            plugins: Arc::new(PluginManager::new()),
            settings: Arc::new(settings),
            registry: Arc::new(registry),
            network: Arc::new(network),
            cache: Arc::new(Cache::default()),
            states: Arc::new(EngineStates::new()),
            metrics: Arc::new(Metrics::new()),
            semantic: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    fn query(text: &str, registry: &EngineRegistry) -> Query {
        Query::parse(text, registry, &[])
    }

    #[test]
    fn engines_of_the_selected_category_are_used() {
        let searcher = searcher(vec![
            FakeEngine::build("web engine", &["general"], 1, true),
            FakeEngine::build("image engine", &["images"], 1, true),
        ]);
        let request = SearchRequest::new(query("cats", &searcher.registry));
        let selected: Vec<String> = searcher
            .select_engines(&request)
            .iter()
            .map(|engine| engine.name().to_string())
            .collect();
        assert_eq!(selected, vec!["web engine".to_string()]);
    }

    #[test]
    fn a_bang_selects_a_single_engine() {
        let searcher = searcher(vec![
            FakeEngine::build("web engine", &["general"], 1, true),
            FakeEngine::build("image engine", &["images"], 1, true),
        ]);
        let registry = searcher.registry.clone();
        let request = SearchRequest::new(query("!image_engine cats", &registry));
        let selected: Vec<String> = searcher
            .select_engines(&request)
            .iter()
            .map(|engine| engine.name().to_string())
            .collect();
        assert_eq!(selected, vec!["image engine".to_string()]);
    }

    /// The engine's own timeout is the ceiling when the deadline is far away.
    #[test]
    fn engine_budget_uses_the_engine_timeout_when_the_deadline_is_far() {
        let now = Instant::now();
        let budget = engine_budget(Duration::from_secs(4), now + Duration::from_secs(30), now);
        assert_eq!(budget, Some(Duration::from_secs(4)));
    }

    /// The search deadline cuts a slow engine short, so one slow engine cannot
    /// hold the whole request open.
    #[test]
    fn engine_budget_is_cut_by_the_search_deadline() {
        let now = Instant::now();
        let budget = engine_budget(
            Duration::from_secs(20),
            now + Duration::from_millis(300),
            now,
        );
        assert_eq!(budget, Some(Duration::from_millis(300)));
    }

    /// A request whose deadline has already passed must not start engines at all.
    /// Before this, a short `timeout_limit` was applied to every engine, so engines
    /// that had already answered in time were also reported as timeouts and their
    /// results were dropped (measured: a 45 ms engine failed under a 100 ms limit).
    #[test]
    fn engine_budget_is_none_after_the_deadline() {
        let now = Instant::now();
        assert_eq!(
            engine_budget(Duration::from_secs(4), now, now + Duration::from_millis(1)),
            None
        );
    }

    #[tokio::test]
    async fn failing_engines_are_reported_as_unresponsive() {
        let searcher = searcher(vec![FakeEngine::build("broken", &["general"], 0, true)]);
        let request = SearchRequest::new(query("cats", &searcher.registry));
        let response = searcher.search(request).await;
        assert!(response.results.is_empty());
        assert_eq!(response.unresponsive_engines.len(), 1);
        assert_eq!(response.unresponsive_engines[0].engine, "broken");
    }

    #[tokio::test]
    async fn no_engine_selected_is_an_error() {
        let searcher = searcher(vec![FakeEngine::build(
            "image engine",
            &["images"],
            1,
            true,
        )]);
        let mut request = SearchRequest::new(query("cats", &searcher.registry));
        request.categories = vec!["videos".to_string()];
        let response = searcher.search(request).await;
        assert!(response.error.is_some());
    }

    #[tokio::test]
    async fn external_bangs_redirect() {
        let searcher = searcher(vec![FakeEngine::build("web engine", &["general"], 1, true)]);
        let request = SearchRequest::new(query("!!yt cats", &searcher.registry));
        let response = searcher.search(request).await;
        assert_eq!(
            response.redirect_url.as_deref(),
            Some("https://www.youtube.com/results?search_query=cats")
        );
    }

    #[tokio::test]
    async fn suspended_engines_are_skipped() {
        let searcher = searcher(vec![FakeEngine::build("broken", &["general"], 0, true)]);
        searcher.states.record_failure(
            "broken",
            FailureKind::TooManyRequests,
            &searcher.settings.search,
        );
        let request = SearchRequest::new(query("cats", &searcher.registry));
        let response = searcher.search(request).await;
        assert_eq!(response.unresponsive_engines.len(), 1);
        assert!(response.unresponsive_engines[0].suspended);
        assert_eq!(
            response.unresponsive_engines[0].error_type,
            "too many requests"
        );
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_similarity_behaves_as_expected() {
        assert!((dot(&[1.0f32, 0.0], &[1.0f32, 0.0]) - 1.0).abs() < 1e-6);
        assert!((dot(&[1.0f32, 0.0], &[0.0f32, 1.0])).abs() < 1e-6);
        assert_eq!(dot(&[], &[1.0]), 0.0);
        assert_eq!(dot(&[1.0f32], &[1.0, 0.0]), 0.0);
    }
}
