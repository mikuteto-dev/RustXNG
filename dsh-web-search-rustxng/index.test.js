import assert from "node:assert/strict";
import test from "node:test";
import { apply } from "./index.js";

function providerFor(config) {
  let provider;
  apply({ web: { registerSearchProvider: (value) => { provider = value; } } }, config);
  return provider;
}

test("search maps RustXNG results and forwards the requested limit", async () => {
  const originalFetch = globalThis.fetch;
  let requested;
  globalThis.fetch = async (url) => {
    requested = new URL(url);
    return new Response(JSON.stringify({ results: [{
      url: "https://example.com",
      title: "Example",
      content: "Snippet",
      publishedDate: "2026-01-01",
    }] }));
  };
  try {
    const result = await providerFor({ baseUrl: "http://127.0.0.1:8888", maxResults: 5, semantic: true })
      .search({ query: "test", maxResults: 2 });
    assert.equal(requested.searchParams.get("q"), "test");
    assert.equal(requested.searchParams.get("max_results"), "2");
    assert.equal(requested.searchParams.get("semantic"), "1");
    assert.deepEqual(result.sources, [{
      url: "https://example.com",
      title: "Example",
      snippet: "Snippet",
      publishedAt: "2026-01-01",
    }]);
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test("search rejects failed RustXNG responses", async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async () => new Response("", { status: 503 });
  try {
    await assert.rejects(providerFor({}).search({ query: "test" }), /HTTP 503/);
  } finally {
    globalThis.fetch = originalFetch;
  }
});
