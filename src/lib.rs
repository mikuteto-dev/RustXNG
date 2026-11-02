//! rustxng — a Rust rewrite of the SearXNG metasearch engine.
//!
//! SearXNG (https://github.com/searxng/searxng) is licensed AGPL-3.0-or-later.
//! This crate is a derivative work and is distributed under the same license;
//! see `LICENSE`, `NOTICE` and `docs/PORTING.md`.
//!
//! The crate is intentionally split into small modules that mirror the upstream
//! layout so that the port stays easy to follow:
//!
//! | this crate            | SearXNG                                    |
//! |-----------------------|--------------------------------------------|
//! | `settings`            | `searx/settings_defaults.py`, `settings.yml` |
//! | `query`               | `searx/query.py`                            |
//! | `results`             | `searx/results.py`, `searx/result_types/*`  |
//! | `engine`              | `searx/engines/__init__.py`                 |
//! | `engines`             | `searx/engines/*.py`                        |
//! | `network`             | `searx/network/*`                           |
//! | `cache`               | `searx/cache.py`                            |
//! | `metrics`             | `searx/metrics/*`                           |
//! | `aggregator`          | `searx/search/processors/online.py`         |
//! | `plugins`             | `searx/plugins/*`                           |
//! | `autocomplete`        | `searx/autocomplete.py`                     |
//! | `web`                 | `searx/webapp.py` + `searx/templates/*`     |

pub mod aggregator;
pub mod answerers;
pub mod autocomplete;
pub mod cache;
pub mod categories;
pub mod content;
pub mod deep_research;
pub mod engine;
pub mod engines;
pub mod external_bangs;
pub mod i18n;
pub mod locales;
pub mod localindex;
pub mod metrics;
pub mod network;
pub mod plugins;
pub mod quality;
pub mod query;
pub mod results;
pub mod semantic;
pub mod settings;
pub mod version;
pub mod web;
pub mod wikidata_infobox;

pub use aggregator::{SearchRequest, SearchResponse, Searcher};
pub use engine::EngineRegistry;
pub use query::Query;
pub use settings::Settings;
