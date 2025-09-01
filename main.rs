use std::collections::{HashMap, HashSet};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::{Client, Url};
use scraper::{Html, Selector};
use serde::Serialize;
use tokio::{
    signal,
    sync::{Mutex, Semaphore},
    time::sleep,
};

// ----------------------------- Config parsing (key=value args) -----------------------------
#[derive(Debug, Clone)]
struct Config {
    seed: String,
    max_depth: usize,
    max_pages: usize,
    concurrency: usize,
    same_origin: bool,
    host_delay_ms: u64,
    out: Option<String>,
    user_agent: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            seed: String::new(),
            max_depth: 2,
            max_pages: 200,
            concurrency: 32,
            same_origin: true,
            host_delay_ms: 150,
            out: None,
            user_agent: "rust-crawler/0.1 (+https://github.com/yourname/rust-crawler)".to_string(),
        }
    }
}

impl Config {
    fn from_key_values() -> Result<Self> {
        let mut cfg = Config::default();
        for arg in std::env::args().skip(1) {
            let (k, v) = if let Some(i) = arg.find('=') {
                (&arg[..i], &arg[i + 1..])
            } else {
                // allow a bare seed value if it's the first non key=value arg
                if cfg.seed.is_empty() && arg.starts_with("http") {
                    cfg.seed = arg;
                    continue;
                } else {
                    anyhow::bail!("unexpected argument: {} (use key=value)", arg);
                }
            };
            match k {
                "seed" => cfg.seed = v.to_string(),
                "max-depth" => cfg.max_depth = v.parse()?,
                "max-pages" => cfg.max_pages = v.parse()?,
                "concurrency" => cfg.concurrency = v.parse()?,
                "same-origin" => cfg.same_origin = matches!(v, "1" | "true" | "yes" | "on"),
                "host-delay-ms" => cfg.host_delay_ms = v.parse()?,
                "out" => cfg.out = Some(v.to_string()),
                "user-agent" => cfg.user_agent = v.to_string(),
                other => anyhow::bail!("unknown key: {}", other),
            }
        }
        if cfg.seed.is_empty() {
            anyhow::bail!("missing seed (provide seed=https://example.com)");
        }
        Ok(cfg)
    }
}

// ----------------------------- Data types -----------------------------
#[derive(Debug, Serialize, Clone)]
struct PageRecord {
    url: String,
    status: Option<u16>,
    content_type: Option<String>,
    depth: usize,
    links: Vec<String>,
}

/// Simple host politeness gate: enforces a minimum delay between requests to the same host
struct HostPoliteness {
    min_delay: Duration,
    // naive per-host last access time; mutex is fine for demo scale
    inner: Arc<Mutex<HashMap<String, tokio::time::Instant>>>,
}

impl HostPoliteness {
    fn new(min_delay: Duration) -> Self {
        Self {
            min_delay,
            inner: Arc::new(Mutex::new(Default::default())),
        }
    }
    async fn wait(&self, url: &Url) {
        use tokio::time::Instant;
        let host = url.host_str().unwrap_or("").to_string();
        let mut map = self.inner.lock().await;
        let now = Instant::now();
        if let Some(prev) = map.get(&host).cloned() {
            let elapsed = now.saturating_duration_since(prev);
            if elapsed < self.min_delay {
                sleep(self.min_delay - elapsed).await;
            }
        }
        map.insert(host, Instant::now());
    }
}

// ----------------------------- Main -----------------------------
#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Config::from_key_values()?;

    let client = Client::builder()
        .user_agent(cfg.user_agent.clone())
        .redirect(reqwest::redirect::Policy::limited(10))
        .timeout(Duration::from_secs(15))
        .build()
        .context("building reqwest client")?;

    let seed_url = Url::parse(&cfg.seed).context("invalid seed URL")?;

    // Shared state
    let seen: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    seen.lock().await.insert(normalize_url(&cfg.seed));

    let results: Arc<Mutex<Vec<PageRecord>>> = Arc::new(Mutex::new(Vec::new()));
    let host_gate = Arc::new(HostPoliteness::new(Duration::from_millis(
        cfg.host_delay_ms,
    )));

    // Frontier channel and inflight counter
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(Url, usize)>();
    let inflight = Arc::new(AtomicUsize::new(0));
    inflight.fetch_add(1, Ordering::SeqCst); // seed counts as in-flight
    tx.send((seed_url, 0)).ok();

    // Concurrency limiter
    let sem = Arc::new(Semaphore::new(cfg.concurrency));

    // Main event loop: single receiver pulls from channel and spawns tasks (bounded by semaphore)
    loop {
        tokio::select! {
            _ = signal::ctrl_c() => {
                eprintln!("ctrl-c: stopping crawl");
                break;
            }
            maybe = rx.recv() => {
                match maybe {
                    Some((url, depth)) => {
                        // If limits already hit, drop this work item
                        if depth > cfg.max_depth || results.lock().await.len() >= cfg.max_pages {
                            inflight.fetch_sub(1, Ordering::SeqCst);
                            continue;
                        }

                        let permit = sem.clone().acquire_owned().await.expect("semaphore");
                        let client = client.clone();
                        let tx = tx.clone();
                        let seen = seen.clone();
                        let results = results.clone();
                        let cfg = cfg.clone();
                        let host_gate = host_gate.clone();
                        let inflight_task = inflight.clone();

                        tokio::spawn(async move {
                            let _p = permit; // hold until end of task

                            match fetch_and_parse(&client, &url, depth, &host_gate).await {
                                Ok(rec) => {
                                    // enqueue children
                                    if depth < cfg.max_depth {
                                        let child_depth = depth + 1;
                                        let mut s = seen.lock().await;
                                        for l in rec.links.iter() {
                                            if s.len() >= cfg.max_pages { break; }
                                            if s.insert(l.clone()) {
                                                if !cfg.same_origin || same_origin(&cfg.seed, l) {
                                                    if let Ok(u) = Url::parse(l) {
                                                        inflight_task.fetch_add(1, Ordering::SeqCst);
                                                        tx.send((u, child_depth)).ok();
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    {
                                        let mut r = results.lock().await;
                                        r.push(rec);
                                    }
                                }
                                Err(err) => {
                                    eprintln!("warn: {} => {}", url, err);
                                }
                            }

                            inflight_task.fetch_sub(1, Ordering::SeqCst);
                        });
                    }
                    None => {
                        // channel closed and nothing more to receive
                        break;
                    }
                }
            }
        }
    }

    // Close sender in this scope; cloned senders in tasks will drop as tasks finish
    drop(tx);

    // Wait until all in-flight tasks finish (or break if already zero)
    while inflight.load(Ordering::SeqCst) != 0 {
        sleep(Duration::from_millis(100)).await;
    }

    // Output
    let out = results.lock().await.clone();
    if let Some(path) = &cfg.out {
        std::fs::write(path, serde_json::to_vec_pretty(&out)?)?;
        eprintln!("wrote {} pages to {}", out.len(), path);
    } else {
        println!("{}", serde_json::to_string_pretty(&out)?);
    }

    Ok(())
}

// ----------------------------- Fetching and parsing -----------------------------
async fn fetch_and_parse(
    client: &Client,
    url: &Url,
    depth: usize,
    host_gate: &Arc<HostPoliteness>,
) -> Result<PageRecord> {
    // politeness per host
    host_gate.wait(url).await;

    let resp = client
        .get(url.clone())
        .send()
        .await
        .with_context(|| format!("GET {}", url))?;
    let status = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let mut links = Vec::new();
    if let Some(ct) = &content_type {
        if ct.starts_with("text/html") {
            let body = resp.text().await?;
            links = extract_links(url, &body);
        } else {
            let _ = resp.bytes().await?; // drain body to reuse conn
        }
    } else {
        let body = resp.text().await.unwrap_or_default();
        links = extract_links(url, &body);
    }

    Ok(PageRecord {
        url: url.to_string(),
        status: Some(status),
        content_type,
        depth,
        links,
    })
}

fn extract_links(base: &Url, body: &str) -> Vec<String> {
    let doc = Html::parse_document(body);
    let sel = Selector::parse("a[href]").unwrap();
    let mut out = Vec::new();
    for el in doc.select(&sel) {
        if let Some(href) = el.value().attr("href") {
            if let Ok(u) = base.join(href) {
                let mut u2 = u.clone();
                u2.set_fragment(None);
                out.push(u2.to_string());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn same_origin(seed: &str, target: &str) -> bool {
    if let (Ok(a), Ok(b)) = (Url::parse(seed), Url::parse(target)) {
        a.origin() == b.origin()
    } else {
        false
    }
}

fn normalize_url(s: &str) -> String {
    Url::parse(s)
        .map(|mut u| {
            u.set_fragment(None);
            u.to_string()
        })
        .unwrap_or_else(|_| s.to_string())
}