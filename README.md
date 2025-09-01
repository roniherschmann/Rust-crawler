## README.md
```markdown
# rust-crawler

A concurrent (async) web crawler in Rust that respects bounded concurrency, deduplicates URLs, optionally restricts to the seed origin, and exports crawl results to JSON.

https://github.com/yourname/rust-crawler

## Features
- Async I/O with Tokio and Reqwest
- Bounded concurrency and a worker pool
- BFS by depth with max-depth
- Deduping of discovered URLs
- Same-origin mode (same-origin) to avoid wandering the whole web
- Simple host politeness (per-host delay)
- JSON output of visited pages: URL, status, content-type, depth, out-links

## Quickstart
```bash
cargo new rust-crawler
cd rust-crawler

# Replace Cargo.toml and src/main.rs with the versions in this repo

make run SEED=https://example.com
```

## CLI
```
seed URL           Seed URL to start crawling (required)
max-depth N        Maximum crawl depth (default: 2)
max-pages N        Soft cap on pages to crawl (default: 200)
concurrency N      Max concurrent requests (default: 32)
same-origin bool   Restrict crawl to seed origin (default: true)
host-delay-ms ms   Min delay between requests to same host (default: 150)
out PATH           Write JSON results to PATH (default: stdout)
user-agent UA      Custom UA string
```

## Output Schema (JSON)
Each element is a page record:
```json
{
  "url": "https://example.com/",
  "status": 200,
  "content_type": "text/html; charset=UTF-8",
  "depth": 1,
  "links": ["https://example.com/about", "https://example.com/contact"]
}
```

## How it Works
- The crawler performs a breadth-first search from the seed. It uses a channel-backed work queue, and a pool of async workers to fetch pages concurrently.
- Each fetched HTML page is parsed with scraper to extract <a href> links, which are normalized and deduplicated.
- In same-origin mode, only links sharing the seed’s origin (scheme + host + port) are enqueued.
- A simple per-host delay avoids hammering the same server.

## Tips
- Run with release for speed.
- Start with max-depth 1 to smoke test connectivity.
- Increase host-delay-ms when crawling public sites.
- Respect robots and site policies when crawling; this demo does not parse robots.txt.

## Roadmap
- Parse and respect robots.txt
- Persistent on-disk frontier and resume
- Filter by content-type, suffix, or regex
- Export to CSV/Parquet in addition to JSON
- Per-host concurrency limits (not just delay)
- HTML meta robots and canonical link handling

## License
MIT
```

---
