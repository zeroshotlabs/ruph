//! Server status page — lightweight metrics and HTML dashboard.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

const QPS_SLOTS: usize = 60;

pub struct ServerStats {
    start_time: Instant,
    active_connections: AtomicUsize,
    total_requests: AtomicU64,
    /// One counter per wall-clock second (ring buffer).
    qps_slots: [AtomicU64; QPS_SLOTS],
    /// Which elapsed-second each slot was last written to.
    qps_epochs: [AtomicU64; QPS_SLOTS],
    ip_hits: Mutex<HashMap<IpAddr, u64>>,
}

impl ServerStats {
    pub fn new() -> Self {
        const ZERO: AtomicU64 = AtomicU64::new(0);
        ServerStats {
            start_time: Instant::now(),
            active_connections: AtomicUsize::new(0),
            total_requests: AtomicU64::new(0),
            qps_slots: [ZERO; QPS_SLOTS],
            qps_epochs: [ZERO; QPS_SLOTS],
            ip_hits: Mutex::new(HashMap::new()),
        }
    }

    pub fn connection_opened(&self) {
        self.active_connections.fetch_add(1, Ordering::Relaxed);
    }

    pub fn connection_closed(&self) {
        self.active_connections.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn record_request(&self, ip: IpAddr) {
        self.total_requests.fetch_add(1, Ordering::Relaxed);

        let now_secs = self.start_time.elapsed().as_secs();
        let idx = (now_secs as usize) % QPS_SLOTS;
        let prev = self.qps_epochs[idx].load(Ordering::Relaxed);
        if prev == now_secs {
            self.qps_slots[idx].fetch_add(1, Ordering::Relaxed);
        } else {
            self.qps_slots[idx].store(1, Ordering::Relaxed);
            self.qps_epochs[idx].store(now_secs, Ordering::Relaxed);
        }

        if let Ok(mut map) = self.ip_hits.lock() {
            *map.entry(ip).or_insert(0) += 1;
        }
    }

    pub fn qps(&self, window: u64) -> f64 {
        let now_secs = self.start_time.elapsed().as_secs();
        let window = window.min(QPS_SLOTS as u64);
        let mut total = 0u64;
        for i in 0..window {
            let check = now_secs.wrapping_sub(i);
            let idx = (check as usize) % QPS_SLOTS;
            if self.qps_epochs[idx].load(Ordering::Relaxed) == check {
                total += self.qps_slots[idx].load(Ordering::Relaxed);
            }
        }
        if window == 0 { 0.0 } else { total as f64 / window as f64 }
    }

    pub fn active_connections(&self) -> usize {
        self.active_connections.load(Ordering::Relaxed)
    }

    pub fn total_requests(&self) -> u64 {
        self.total_requests.load(Ordering::Relaxed)
    }

    pub fn uptime(&self) -> std::time::Duration {
        self.start_time.elapsed()
    }

    pub fn ip_hits(&self) -> Vec<(IpAddr, u64)> {
        let map = self.ip_hits.lock().unwrap_or_else(|e| e.into_inner());
        let mut hits: Vec<_> = map.iter().map(|(ip, c)| (*ip, *c)).collect();
        hits.sort_by(|a, b| b.1.cmp(&a.1));
        hits
    }
}

pub fn render_status_page(stats: &ServerStats) -> String {
    let uptime = stats.uptime();
    let days = uptime.as_secs() / 86400;
    let hours = (uptime.as_secs() % 86400) / 3600;
    let minutes = (uptime.as_secs() % 3600) / 60;
    let seconds = uptime.as_secs() % 60;

    let uptime_str = if days > 0 {
        format!("{}d {}h {}m {}s", days, hours, minutes, seconds)
    } else {
        format!("{}h {}m {}s", hours, minutes, seconds)
    };

    let ip_hits = stats.ip_hits();
    let ip_rows: String = ip_hits.iter()
        .take(50)
        .map(|(ip, count)| format!(
            "            <tr><td>{}</td><td class=\"num\">{}</td></tr>", ip, count
        ))
        .collect::<Vec<_>>()
        .join("\n");

    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");

    format!(r#"<!DOCTYPE html>
<html>
<head>
<title>ruph server status</title>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>
body {{ font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, monospace;
       margin: 2rem; background: #1a1a2e; color: #e0e0e0; }}
h1 {{ color: #e94560; margin-bottom: 0.3rem; }}
h2 {{ color: #ccc; margin-top: 2rem; }}
.sub {{ color: #666; font-size: 0.85rem; margin-bottom: 1.5rem; }}
.stats {{ display: flex; flex-wrap: wrap; gap: 0.75rem; }}
.stat {{ background: #16213e; padding: 1rem 1.5rem; border-radius: 8px; min-width: 170px; }}
.stat .label {{ font-size: 0.8rem; color: #888; text-transform: uppercase; letter-spacing: 0.05em; }}
.stat .value {{ font-size: 1.8rem; font-weight: bold; color: #e94560; margin-top: 0.2rem; }}
table {{ border-collapse: collapse; margin-top: 0.5rem; width: 100%; max-width: 500px; }}
th, td {{ padding: 0.35rem 1rem; text-align: left; border-bottom: 1px solid #2a2a4a; }}
th {{ background: #16213e; color: #e94560; font-size: 0.8rem; text-transform: uppercase; }}
td.num {{ text-align: right; font-variant-numeric: tabular-nums; }}
.footer {{ margin-top: 2rem; font-size: 0.75rem; color: #444; }}
</style>
</head>
<body>
<h1>ruph</h1>
<div class="sub">server status &mdash; {now}</div>
<div class="stats">
    <div class="stat">
        <div class="label">Active Connections</div>
        <div class="value">{active}</div>
    </div>
    <div class="stat">
        <div class="label">Req/s (10s)</div>
        <div class="value">{qps_10:.1}</div>
    </div>
    <div class="stat">
        <div class="label">Req/s (60s)</div>
        <div class="value">{qps_60:.1}</div>
    </div>
    <div class="stat">
        <div class="label">Total Requests</div>
        <div class="value">{total}</div>
    </div>
    <div class="stat">
        <div class="label">Uptime</div>
        <div class="value">{uptime}</div>
    </div>
    <div class="stat">
        <div class="label">Unique IPs</div>
        <div class="value">{unique_ips}</div>
    </div>
</div>

<h2>Hits by Source IP</h2>
<table>
    <tr><th>IP Address</th><th style="text-align:right">Requests</th></tr>
{ip_rows}
</table>

<div class="footer">ruph/0.1.0</div>
</body>
</html>"#,
        now = now,
        active = stats.active_connections(),
        qps_10 = stats.qps(10),
        qps_60 = stats.qps(60),
        total = stats.total_requests(),
        uptime = uptime_str,
        unique_ips = ip_hits.len(),
        ip_rows = ip_rows,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_connection_tracking() {
        let stats = ServerStats::new();
        assert_eq!(stats.active_connections(), 0);
        stats.connection_opened();
        stats.connection_opened();
        assert_eq!(stats.active_connections(), 2);
        stats.connection_closed();
        assert_eq!(stats.active_connections(), 1);
    }

    #[test]
    fn test_request_counting() {
        let stats = ServerStats::new();
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        stats.record_request(ip);
        stats.record_request(ip);
        assert_eq!(stats.total_requests(), 2);
        let hits = stats.ip_hits();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1, 2);
    }

    #[test]
    fn test_multiple_ips() {
        let stats = ServerStats::new();
        let ip1: IpAddr = "10.0.0.1".parse().unwrap();
        let ip2: IpAddr = "10.0.0.2".parse().unwrap();
        for _ in 0..5 { stats.record_request(ip1); }
        for _ in 0..3 { stats.record_request(ip2); }
        let hits = stats.ip_hits();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0], (ip1, 5)); // sorted desc
        assert_eq!(hits[1], (ip2, 3));
    }

    #[test]
    fn test_qps_current_second() {
        let stats = ServerStats::new();
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        for _ in 0..100 {
            stats.record_request(ip);
        }
        let qps = stats.qps(1);
        assert!(qps >= 99.0);
    }

    #[test]
    fn test_render_does_not_panic() {
        let stats = ServerStats::new();
        let ip: IpAddr = "192.168.1.1".parse().unwrap();
        stats.record_request(ip);
        let html = render_status_page(&stats);
        assert!(html.contains("ruph"));
        assert!(html.contains("Active Connections"));
        assert!(html.contains("192.168.1.1"));
    }
}
