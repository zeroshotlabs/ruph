//! Full request logging to SQLite (WAL mode).
//!
//! Enabled by `log_full = /path/to/requests.db` in ruph.ini [server].
//! All inserts are done via `spawn_blocking` so the async runtime is never stalled.

use anyhow::Result;
use rusqlite::{Connection, params};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tracing::warn;

/// Holds the SQLite connection behind a std Mutex (rusqlite is not Send).
pub struct RequestLogger {
    conn: Mutex<Connection>,
    db_path: PathBuf,
}

impl RequestLogger {
    /// Open (or create) the database, enable WAL, create tables/indexes.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(path)?;

        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS requests (
                id              INTEGER PRIMARY KEY,
                ts              TEXT    NOT NULL,
                ts_epoch_ms     INTEGER NOT NULL,
                ip              TEXT    NOT NULL,
                port            INTEGER NOT NULL,
                method          TEXT    NOT NULL,
                host            TEXT    NOT NULL,
                path            TEXT    NOT NULL,
                query           TEXT,
                protocol        TEXT,
                status          INTEGER,
                response_size   INTEGER,
                duration_us     INTEGER,
                tls             INTEGER NOT NULL DEFAULT 0,
                sni             TEXT,
                http_version    TEXT,
                request_headers TEXT,
                user_agent      TEXT,
                referer         TEXT,
                accept          TEXT,
                accept_language TEXT,
                accept_encoding TEXT,
                content_type    TEXT,
                content_length  INTEGER,
                cookie          TEXT,
                authorization   TEXT,
                x_forwarded_for TEXT,
                origin          TEXT,
                response_headers TEXT,
                vhost           TEXT,
                -- post-processing columns (filled later by external tools)
                geo_country     TEXT,
                geo_city        TEXT,
                geo_region      TEXT,
                geo_lat         REAL,
                geo_lon         REAL,
                asn             TEXT,
                asn_org         TEXT,
                bot_flag        INTEGER,
                bot_name        TEXT,
                abuse_score     REAL,
                notes           TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_req_ts      ON requests(ts);
            CREATE INDEX IF NOT EXISTS idx_req_epoch    ON requests(ts_epoch_ms);
            CREATE INDEX IF NOT EXISTS idx_req_ip       ON requests(ip);
            CREATE INDEX IF NOT EXISTS idx_req_host     ON requests(host);
            CREATE INDEX IF NOT EXISTS idx_req_status   ON requests(status);
            CREATE INDEX IF NOT EXISTS idx_req_method   ON requests(method);
            CREATE INDEX IF NOT EXISTS idx_req_path     ON requests(path);
            CREATE INDEX IF NOT EXISTS idx_req_ua       ON requests(user_agent);
            CREATE INDEX IF NOT EXISTS idx_req_tls      ON requests(tls);
            "
        )?;

        eprintln!("  log_full: {}", path.display());

        Ok(RequestLogger {
            conn: Mutex::new(conn),
            db_path: path.to_path_buf(),
        })
    }

    /// Path to the database file (for diagnostics).
    #[allow(dead_code)]
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Insert a request record. Call from async context via spawn_blocking.
    pub fn insert(&self, rec: &RequestRecord) {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(e) => {
                warn!("log_full: mutex poisoned: {}", e);
                return;
            }
        };

        let result = conn.execute(
            "INSERT INTO requests (
                ts, ts_epoch_ms, ip, port, method, host, path, query,
                protocol, status, response_size, duration_us,
                tls, sni, http_version,
                request_headers,
                user_agent, referer, accept, accept_language, accept_encoding,
                content_type, content_length, cookie, authorization,
                x_forwarded_for, origin,
                response_headers, vhost
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                ?9, ?10, ?11, ?12,
                ?13, ?14, ?15,
                ?16,
                ?17, ?18, ?19, ?20, ?21,
                ?22, ?23, ?24, ?25,
                ?26, ?27,
                ?28, ?29
            )",
            params![
                rec.ts,
                rec.ts_epoch_ms,
                rec.ip,
                rec.port,
                rec.method,
                rec.host,
                rec.path,
                rec.query,
                rec.protocol,
                rec.status,
                rec.response_size,
                rec.duration_us,
                rec.tls as i32,
                rec.sni,
                rec.http_version,
                rec.request_headers,
                rec.user_agent,
                rec.referer,
                rec.accept,
                rec.accept_language,
                rec.accept_encoding,
                rec.content_type,
                rec.content_length,
                rec.cookie,
                rec.authorization,
                rec.x_forwarded_for,
                rec.origin,
                rec.response_headers,
                rec.vhost,
            ],
        );

        if let Err(e) = result {
            warn!("log_full insert error: {}", e);
        }
    }
}

/// All the raw data we capture per request.
#[derive(Debug, Clone)]
pub struct RequestRecord {
    pub ts: String,
    pub ts_epoch_ms: i64,
    pub ip: String,
    pub port: u16,
    pub method: String,
    pub host: String,
    pub path: String,
    pub query: Option<String>,
    pub protocol: Option<String>,
    pub status: Option<u16>,
    pub response_size: Option<i64>,
    pub duration_us: Option<i64>,
    pub tls: bool,
    pub sni: Option<String>,
    pub http_version: Option<String>,
    pub request_headers: Option<String>,
    pub user_agent: Option<String>,
    pub referer: Option<String>,
    pub accept: Option<String>,
    pub accept_language: Option<String>,
    pub accept_encoding: Option<String>,
    pub content_type: Option<String>,
    pub content_length: Option<i64>,
    pub cookie: Option<String>,
    pub authorization: Option<String>,
    pub x_forwarded_for: Option<String>,
    pub origin: Option<String>,
    pub response_headers: Option<String>,
    pub vhost: Option<String>,
}

/// Snapshot of request data captured before the request is consumed.
/// Call `into_record()` after the response is ready to produce the final record.
pub struct RequestSnapshot {
    ts: chrono::DateTime<chrono::Local>,
    ip: String,
    port: u16,
    method: String,
    host: String,
    path: String,
    query: Option<String>,
    protocol: String,
    tls: bool,
    sni: Option<String>,
    http_version: String,
    request_headers_json: Option<String>,
    user_agent: Option<String>,
    referer: Option<String>,
    accept: Option<String>,
    accept_language: Option<String>,
    accept_encoding: Option<String>,
    content_type: Option<String>,
    content_length: Option<i64>,
    cookie: Option<String>,
    authorization: Option<String>,
    x_forwarded_for: Option<String>,
    origin: Option<String>,
    vhost: String,
}

fn headers_to_json(headers: &hyper::HeaderMap) -> Option<String> {
    let mut map = serde_json::Map::new();
    for (name, value) in headers.iter() {
        if let Ok(v) = value.to_str() {
            let key = name.as_str().to_string();
            if let Some(existing) = map.get_mut(&key) {
                if let serde_json::Value::String(s) = existing {
                    s.push_str(", ");
                    s.push_str(v);
                }
            } else {
                map.insert(key, serde_json::Value::String(v.to_string()));
            }
        }
    }
    if map.is_empty() { None } else { serde_json::to_string(&map).ok() }
}

impl RequestSnapshot {
    /// Capture all request data before the request body is consumed.
    pub fn capture(
        req: &hyper::Request<hyper::body::Incoming>,
        remote_addr: std::net::SocketAddr,
        is_tls: bool,
        domain: &str,
    ) -> Self {
        let hdr = |name: &str| -> Option<String> {
            req.headers().get(name).and_then(|v| v.to_str().ok()).map(|s| s.to_string())
        };

        let content_length: Option<i64> = req.headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse().ok());

        let request_headers_json = headers_to_json(req.headers());

        let uri = req.uri();
        let host = req.headers().get("host")
            .and_then(|v| v.to_str().ok())
            .or_else(|| uri.authority().map(|a| a.as_str()))
            .unwrap_or("-")
            .to_string();

        RequestSnapshot {
            ts: chrono::Local::now(),
            ip: remote_addr.ip().to_string(),
            port: remote_addr.port(),
            method: req.method().to_string(),
            host: host.clone(),
            path: uri.path().to_string(),
            query: uri.query().map(|s| s.to_string()),
            protocol: if is_tls { "https".into() } else { "http".into() },
            tls: is_tls,
            sni: if is_tls { Some(domain.to_string()) } else { None },
            http_version: format!("{:?}", req.version()),
            request_headers_json,
            user_agent: hdr("user-agent"),
            referer: hdr("referer"),
            accept: hdr("accept"),
            accept_language: hdr("accept-language"),
            accept_encoding: hdr("accept-encoding"),
            content_type: hdr("content-type"),
            content_length,
            cookie: hdr("cookie"),
            authorization: hdr("authorization"),
            x_forwarded_for: hdr("x-forwarded-for"),
            origin: hdr("origin"),
            vhost: host,
        }
    }

    /// Convert the snapshot into a full record once the response is available.
    pub fn into_record(
        self,
        status: u16,
        response_headers: &hyper::HeaderMap,
        response_size: Option<i64>,
        duration: std::time::Duration,
    ) -> RequestRecord {
        let resp_headers_json = headers_to_json(response_headers);

        RequestRecord {
            ts: self.ts.format("%Y-%m-%dT%H:%M:%S%.3f%:z").to_string(),
            ts_epoch_ms: self.ts.timestamp_millis(),
            ip: self.ip,
            port: self.port,
            method: self.method,
            host: self.host,
            path: self.path,
            query: self.query,
            protocol: Some(self.protocol),
            status: Some(status),
            response_size,
            duration_us: Some(duration.as_micros() as i64),
            tls: self.tls,
            sni: self.sni,
            http_version: Some(self.http_version),
            request_headers: self.request_headers_json,
            user_agent: self.user_agent,
            referer: self.referer,
            accept: self.accept,
            accept_language: self.accept_language,
            accept_encoding: self.accept_encoding,
            content_type: self.content_type,
            content_length: self.content_length,
            cookie: self.cookie,
            authorization: self.authorization,
            x_forwarded_for: self.x_forwarded_for,
            origin: self.origin,
            response_headers: resp_headers_json,
            vhost: Some(self.vhost),
        }
    }
}
