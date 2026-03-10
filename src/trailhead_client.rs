//! Async Trailhead log ingestion client.
//!
//! Serializes request records as NDJSON and POSTs them to the Trailhead API
//! in batches.  Each owner gets its own buffer; a background task flushes
//! buffers when they hit a size limit or a time deadline.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::request_log::RequestRecord;

const MAX_BATCH_LINES: usize = 400;
const FLUSH_INTERVAL_SECS: u64 = 5;

/// Per-owner buffer of NDJSON lines waiting to be shipped.
struct OwnerBuf {
    lines: Vec<String>,
}

/// Shared state across the flush task and the request path.
struct Inner {
    api_url: String,
    api_key: String,
    client: reqwest::Client,
    buffers: HashMap<String, OwnerBuf>,
}

/// Handle returned to request handlers for non-blocking log submission.
pub struct TrailheadClient {
    inner: Arc<Mutex<Inner>>,
    /// Domain -> owner resolution (exact match)
    domain_owners: HashMap<String, String>,
    /// Prefix -> owner resolution (longest prefix wins)
    prefix_owners: Vec<(String, String)>,
    /// Fallback owner when no per-domain match
    default_owner: Option<String>,
}

impl TrailheadClient {
    pub fn new(
        api_url: String,
        api_key: String,
        domain_owners: HashMap<String, String>,
        prefix_owners: Vec<(String, String)>,
        default_owner: Option<String>,
    ) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .pool_max_idle_per_host(2)
            .build()
            .expect("failed to build reqwest client");

        let inner = Arc::new(Mutex::new(Inner {
            api_url: api_url.trim_end_matches('/').to_string(),
            api_key,
            client,
            buffers: HashMap::new(),
        }));

        eprintln!("  trailhead: enabled");
        for (d, o) in &domain_owners {
            eprintln!("  trailhead: {} -> {}", d, o);
        }
        for (p, o) in &prefix_owners {
            eprintln!("  trailhead: {}* -> {}", p, o);
        }
        if let Some(ref d) = default_owner {
            eprintln!("  trailhead: * -> {}", d);
        }

        TrailheadClient { inner, domain_owners, prefix_owners, default_owner }
    }

    /// Resolve which owner a domain maps to.  Returns None if no match.
    pub fn resolve_owner(&self, domain: &str) -> Option<&str> {
        let bare = domain.split(':').next().unwrap_or(domain);

        // 1. Exact domain match
        if let Some(owner) = self.domain_owners.get(bare) {
            return Some(owner.as_str());
        }

        // 2. Longest prefix match
        let mut best: Option<&str> = None;
        let mut best_len = 0;
        for (prefix, owner) in &self.prefix_owners {
            if prefix.len() > best_len
                && bare.starts_with(prefix.as_str())
                && (bare.len() == prefix.len()
                    || bare.as_bytes().get(prefix.len()) == Some(&b'.'))
            {
                best = Some(owner.as_str());
                best_len = prefix.len();
            }
        }
        if best.is_some() {
            return best;
        }

        // 3. Default owner
        self.default_owner.as_deref()
    }

    /// Queue a request record for shipping.  Non-blocking; returns immediately.
    pub fn submit(&self, owner: &str, rec: &RequestRecord) {
        let line = record_to_ndjson(rec);
        let owner = owner.to_string();
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let mut guard = inner.lock().await;
            let buf = guard.buffers.entry(owner.clone()).or_insert_with(|| OwnerBuf {
                lines: Vec::with_capacity(MAX_BATCH_LINES),
            });
            buf.lines.push(line);
            if buf.lines.len() >= MAX_BATCH_LINES {
                let lines = std::mem::take(&mut buf.lines);
                let url = format!("{}/ingest?owner={}", guard.api_url, owner);
                let client = guard.client.clone();
                let api_key = guard.api_key.clone();
                drop(guard);
                tokio::spawn(async move {
                    ship_batch(&client, &url, &api_key, &lines).await;
                });
            }
        });
    }

    /// Start the periodic flush background task.
    pub fn start_flush_task(&self) {
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(
                std::time::Duration::from_secs(FLUSH_INTERVAL_SECS),
            );
            loop {
                interval.tick().await;
                let mut guard = inner.lock().await;
                let api_url = guard.api_url.clone();
                let api_key = guard.api_key.clone();
                let client = guard.client.clone();
                let mut batches: Vec<(String, Vec<String>)> = Vec::new();
                for (owner, buf) in guard.buffers.iter_mut() {
                    if !buf.lines.is_empty() {
                        batches.push((owner.clone(), std::mem::take(&mut buf.lines)));
                    }
                }
                drop(guard);
                for (owner, lines) in batches {
                    let url = format!("{}/ingest?owner={}", api_url, owner);
                    let c = client.clone();
                    let k = api_key.clone();
                    tokio::spawn(async move {
                        ship_batch(&c, &url, &k, &lines).await;
                    });
                }
            }
        });
    }
}

fn record_to_ndjson(rec: &RequestRecord) -> String {
    let mut obj = serde_json::Map::new();
    obj.insert("timestamp".into(), serde_json::Value::Number(rec.ts_epoch_ms.into()));
    obj.insert("ip".into(), rec.ip.clone().into());
    obj.insert("port".into(), serde_json::Value::Number(rec.port.into()));
    obj.insert("method".into(), rec.method.clone().into());
    obj.insert("host".into(), rec.host.clone().into());
    obj.insert("path".into(), rec.path.clone().into());

    macro_rules! opt_str {
        ($field:ident) => {
            if let Some(ref v) = rec.$field { obj.insert(stringify!($field).into(), v.clone().into()); }
        };
    }
    macro_rules! opt_num {
        ($field:ident) => {
            if let Some(v) = rec.$field { obj.insert(stringify!($field).into(), serde_json::Value::Number(v.into())); }
        };
    }

    opt_str!(query);
    opt_str!(protocol);
    opt_num!(status);
    opt_num!(response_size);
    opt_num!(duration_us);
    obj.insert("tls".into(), serde_json::Value::Bool(rec.tls));
    opt_str!(sni);
    opt_str!(http_version);
    // Parse stored JSON headers back into objects
    if let Some(ref h) = rec.request_headers {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(h) {
            obj.insert("request_headers".into(), v);
        }
    }
    opt_str!(user_agent);
    opt_str!(referer);
    opt_str!(accept);
    opt_str!(accept_language);
    opt_str!(accept_encoding);
    opt_str!(content_type);
    opt_num!(content_length);
    opt_str!(cookie);
    opt_str!(authorization);
    opt_str!(x_forwarded_for);
    opt_str!(origin);
    if let Some(ref h) = rec.response_headers {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(h) {
            obj.insert("response_headers".into(), v);
        }
    }
    opt_str!(vhost);

    serde_json::to_string(&serde_json::Value::Object(obj)).unwrap_or_default()
}

async fn ship_batch(client: &reqwest::Client, url: &str, api_key: &str, lines: &[String]) {
    let body = lines.join("\n");
    let result = client
        .post(url)
        .header("x-api-key", api_key)
        .header("content-type", "application/x-ndjson")
        .body(body)
        .send()
        .await;
    match result {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => {
            eprintln!("trailhead: {} returned {}", url, resp.status());
        }
        Err(e) => {
            eprintln!("trailhead: POST {} failed: {}", url, e);
        }
    }
}
