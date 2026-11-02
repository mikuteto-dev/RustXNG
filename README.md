# RustXNG

**A self-hosted, SSRF-hardened web search and grounding engine for local AI agents.**

No API keys. No fees. One request returns ranked results **and the page text**, so an agent can answer from sources instead of guessing.

[日本語](README.ja.md)

[![CI](https://github.com/sahenjp/RustXNG/actions/workflows/ci.yml/badge.svg)](https://github.com/sahenjp/RustXNG/actions/workflows/ci.yml)
[![Nightly benchmark](https://github.com/sahenjp/RustXNG/actions/workflows/nightly-bench.yml/badge.svg)](https://github.com/sahenjp/RustXNG/actions/workflows/nightly-bench.yml)
[![License: AGPL-3.0-or-later](https://img.shields.io/badge/license-AGPL--3.0--or--later-blue.svg)](LICENSE)

## Why this exists

An agent searches, reads, and decides — then does it again. What it needs underneath is not another search UI but a **retrieval layer**:

| What an agent needs | What RustXNG does |
| --- | --- |
| No keys, no per-seat billing | Free, keyless, self-hosted |
| Stay inside your network | Runs on your machine; search terms never leave it |
| Fast enough to call 50 times a task | **0.01 s** on a repeated query (cached) |
| Text to ground on | **~32,000 characters** from the top 5 pages in one request |
| Survive one dead source | 180 engines in parallel, per-engine suspension and rest |
| Safe with untrusted URLs | Public addresses only (**SSRF guard**) |
| One local process | HTTP search and MCP share the same server |

## Install

### 1. Prebuilt binary (fastest)

Download the archive for your platform from [Releases](https://github.com/sahenjp/RustXNG/releases/latest) and put `rustxng` on your `PATH`.

```bash
curl -fsSL -o rustxng \
  https://github.com/sahenjp/RustXNG/releases/latest/download/rustxng-v0.25.0-x86_64-unknown-linux-musl
chmod +x rustxng && ./rustxng --version
```

> Check the `.sha256` published next to each asset before you run it.

### 2. Cargo

```bash
cargo install --git https://github.com/sahenjp/RustXNG.git --tag v0.25.0
```

This installs two binaries: `rustxng` (the server) and `rustxng-index` (the optional local index).

### 3. Docker

```bash
docker run -p 8888:8888 ghcr.io/sahenjp/rustxng:latest
```

Published from v0.26.0 onwards. To build it yourself instead:

```bash
git clone https://github.com/sahenjp/RustXNG.git && cd RustXNG
docker build -t rustxng . && docker run -p 8888:8888 rustxng
```

Bind your own settings with `-v "$PWD/settings.toml:/app/settings.toml:ro"`.

### 4. From source

```bash
git clone https://github.com/sahenjp/RustXNG.git
cd RustXNG
cargo build --release
./target/release/rustxng
```

Requires Rust 1.85 or newer. No system libraries beyond a C toolchain.

## Quickstart

```bash
# start on 127.0.0.1:8888
rustxng &

# search
curl "http://127.0.0.1:8888/search?q=tokio+runtime&format=json"

# search + page text for grounding
curl "http://127.0.0.1:8888/search?q=tokio+runtime&format=json&include_raw_content=1"

# optional semantic re-ranking on macOS 15+ (first use downloads about 550 MB)
curl "http://127.0.0.1:8888/search?q=tokio+runtime&format=json&semantic=1"
```

The same process exposes a stateless MCP endpoint at `http://127.0.0.1:8888/mcp` with a `web_search` tool. Semantic re-ranking uses EmbeddingGemma 2 through Core ML and loads only when `semantic=1` is requested.

Response shape (SearXNG compatible):

```json
{"results": [{"title": "...", "url": "...", "content": "...", "raw_content": "..."}]}
```

The browser UI is at <http://127.0.0.1:8888/>.

## How it compares

100 questions with known answer pages, measured on the same machine:

| | RustXNG | Exa | Tavily |
| --- | ---: | ---: | ---: |
| hit@1 (right page first) | **79–83** | 77 | 34 |
| hit@5 | **92–96** | 91 | 79 |
| MRR | **0.853–0.889** | 0.840 | 0.514 |
| Cost | **$0** | paid | $8/1k |
| Page text per request | **~32,000 chars** | ~9,000 | ~1,200 |
| Repeat query | **0.01 s** | 0.6 s | ~1 s |
| Cold query | 0.8–1.6 s | **0.6 s** | ~1 s |

Two caveats, stated plainly: the numbers move by a few points between runs, and **Google is unavailable in this build** (its WML endpoint now returns 403, the HTML endpoint requires JavaScript, and headless browsers are detected). See [docs/COMPARISON-EXA.md](docs/COMPARISON-EXA.md) for a plain-language version.

## What you get

- **226 engine types** ported from SearXNG, 352 settings entries, 180 enabled by default
- **Ranking** that beats raw engine order: term coverage, canonical-site authority, identifier match (RFC numbers), thumbnail-free deduplication
- **Resilience**: per-engine timeouts, suspension with backoff, and automatic rest that drops engines which keep answering nothing
- **Optional local index** (`rustxng-index`): crawl your own corpus, rank with BM25, merge into results — experimental, off by default
- **Ops**: `/healthz`, OpenMetrics `/metrics`, response cache, image/favicon proxy, bot rate limiting, `RUSTXNG_IDLE_TIMEOUT` for sidecars

## Documentation

| | |
| --- | --- |
| [docs/AI-INFRA.md](docs/AI-INFRA.md) | Why this is agent infrastructure, and why tests and audits matter |
| [docs/COMPARISON-EXA.md](docs/COMPARISON-EXA.md) | The comparison above, explained without jargon |
| [docs/BENCHMARK.md](docs/BENCHMARK.md) | How the numbers were produced, including the variance |
| [docs/COVERAGE.md](docs/COVERAGE.md) | Every engine, mapped to its SearXNG counterpart |
| [docs/COMPATIBILITY.md](docs/COMPATIBILITY.md) | Where this matches SearXNG and where it differs |
| [docs/LOCAL-INDEX.md](docs/LOCAL-INDEX.md) | The experimental local index (crawl + BM25) |
| [docs/PORTING.md](docs/PORTING.md) | Internals and configuration |
| [SECURITY.md](SECURITY.md) | Vulnerability scan results and known advisories |

## Contributing

Issues and pull requests are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md). The rules are short: run `cargo fmt`, `cargo clippy`, `cargo test`, and keep the nightly benchmark green.

Good first issues live in the tracker; the parsers under `src/engines/` are deliberately small and each has a fixture test.

## License

**AGPL-3.0-or-later**. This is a port of [SearXNG](https://github.com/searxng/searxng); attribution and the exact scope of the port are in [NOTICE](NOTICE). If you run a modified version as a network service, AGPL section 13 asks you to offer its source to your users.
