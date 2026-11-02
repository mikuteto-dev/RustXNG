export const name = "web-search-rustxng";
export const inject = ["web"];

export function apply(ctx, config = {}) {
  const baseUrl = config.baseUrl || "http://127.0.0.1:8888";
  const maxResults = config.maxResults || 5;
  const semantic = config.semantic === true;
  ctx.web.registerSearchProvider({
    id: config.providerId || "rustxng",
    available: () => Boolean(baseUrl),
    async search(request, signal) {
      const url = new URL("/search", baseUrl);
      url.searchParams.set("q", request.query);
      url.searchParams.set("format", "json");
      url.searchParams.set("max_results", String(request.maxResults ?? maxResults));
      if (semantic) url.searchParams.set("semantic", "1");
      const response = await fetch(url, { signal });
      if (!response.ok) throw new Error(`RustXNG returned HTTP ${response.status}`);
      const data = await response.json();
      return {
        sources: (data.results ?? []).map(({ url: sourceUrl, title, content, publishedDate }) => ({
          url: sourceUrl,
          ...(title ? { title } : {}),
          snippet: content ?? "",
          ...(publishedDate ? { publishedAt: publishedDate } : {}),
        })),
      };
    },
  });
}
