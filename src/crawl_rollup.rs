//! Background crawl log aggregation and IP enrichment.
//!
//! Periodically rolls up raw tab-delimited crawl logs into hourly/daily JSON
//! summaries, then enriches top IPs with RDAP/GeoIP data.

use anyhow::Result;
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::Path;
use tokio::time::{interval, Duration};
use tracing::{debug, info, warn};

// ── Data structures ────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Default)]
pub struct HourlyAgg {
    pub date: String,
    pub hour: String,
    pub total: u64,
    pub bots: u64,
    pub humans: u64,
    pub suspects: u64,
    pub unique_ips: usize,
    pub ips: HashMap<String, u64>,
    pub paths: HashMap<String, u64>,
    pub uas: HashMap<String, u64>,
    pub refs: HashMap<String, u64>,
    pub hosts: HashMap<String, u64>,
    pub crawlers: HashMap<String, u64>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct DailyAgg {
    pub date: String,
    pub total: u64,
    pub bots: u64,
    pub humans: u64,
    pub suspects: u64,
    pub unique_ips: usize,
    pub hourly: HashMap<String, u64>,
    pub ips: HashMap<String, u64>,
    pub paths: HashMap<String, u64>,
    pub hosts: HashMap<String, u64>,
    pub crawlers: HashMap<String, u64>,
    pub refs: HashMap<String, u64>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct IpInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asn: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub org: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
}

// ── Crawler identification (mirrors PHP identify_crawler) ──────────────────

fn identify_crawler(ua_lower: &str) -> &'static str {
    if ua_lower.contains("googlebot") { return "Googlebot"; }
    if ua_lower.contains("bingbot") { return "Bingbot"; }
    if ua_lower.contains("gptbot") { return "GPTBot"; }
    if ua_lower.contains("claudebot") { return "ClaudeBot"; }
    if ua_lower.contains("chatgpt") { return "ChatGPT"; }
    if ua_lower.contains("bytespider") { return "ByteSpider"; }
    if ua_lower.contains("ahref") { return "AhrefsBot"; }
    if ua_lower.contains("semrush") { return "SemrushBot"; }
    if ua_lower.contains("yandex") { return "YandexBot"; }
    if ua_lower.contains("baidu") { return "Baiduspider"; }
    if ua_lower.contains("petalbot") { return "PetalBot"; }
    if ua_lower.contains("applebot") { return "Applebot"; }
    if ua_lower.contains("facebookexternalhit") { return "Facebook"; }
    if ua_lower.contains("duckduckbot") { return "DuckDuckBot"; }
    if ua_lower.contains("ccbot") { return "CCBot"; }
    if ua_lower.contains("linkedinbot") { return "LinkedIn"; }
    if ua_lower.contains("twitterbot") { return "Twitter"; }
    if ua_lower.contains("python") { return "Python"; }
    if ua_lower.contains("curl") { return "curl"; }
    if ua_lower.contains("wget") { return "wget"; }
    if ua_lower.contains("go-http") { return "Go-HTTP"; }
    if ua_lower.contains("scrapy") { return "Scrapy"; }
    if ua_lower.contains("zgrab") { return "ZGrab"; }
    if ua_lower.contains("masscan") { return "Masscan"; }
    if ua_lower.contains("censys") { return "Censys"; }
    if ua_lower.contains("shodan") { return "Shodan"; }
    if ua_lower.contains("nuclei") { return "Nuclei"; }
    if ua_lower.contains("nikto") { return "Nikto"; }
    if ua_lower.contains("java/") { return "Java Client"; }
    if ua_lower.contains("okhttp") { return "OkHttp"; }
    if ua_lower.contains("httpclient") { return "HTTPClient"; }
    if ua_lower.contains("bot") { return "Other Bot"; }
    if ua_lower.contains("crawl") { return "Other Crawler"; }
    if ua_lower.contains("spider") { return "Other Spider"; }
    "Human"
}

// ── Hourly rollup ──────────────────────────────────────────────────────────

fn rollup_hour(crawl_dir: &str, agg_dir: &str, date: &str, hour: &str) -> Result<bool> {
    let agg_path = format!("{}/hourly/{}_{}.json", agg_dir, date, hour);
    if Path::new(&agg_path).exists() {
        return Ok(false); // already done
    }

    let raw_path = format!("{}/{}.log", crawl_dir, date);
    if !Path::new(&raw_path).exists() {
        return Ok(false);
    }

    let file = std::fs::File::open(&raw_path)?;
    let reader = BufReader::new(file);

    let mut agg = HourlyAgg {
        date: date.to_string(),
        hour: hour.to_string(),
        ..Default::default()
    };

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 6 {
            continue;
        }

        // Filter by hour: first 2 chars of HH:MM:SS
        if parts[0].len() < 2 || &parts[0][..2] != hour {
            continue;
        }

        agg.total += 1;
        let ip = parts[1];
        let host = parts[2];
        let uri = parts[4];
        let ttype = parts[5];
        let ua = if parts.len() > 6 { parts[6] } else { "" };
        let referer = if parts.len() > 7 { parts[7] } else { "" };

        match ttype {
            "bot" => {
                agg.bots += 1;
                let ua_lower = ua.to_lowercase();
                let cname = identify_crawler(&ua_lower);
                *agg.crawlers.entry(cname.to_string()).or_insert(0) += 1;
            }
            "suspect" => {
                agg.suspects += 1;
            }
            _ => {
                agg.humans += 1;
            }
        }

        if !ip.is_empty() {
            *agg.ips.entry(ip.to_string()).or_insert(0) += 1;
        }
        *agg.hosts.entry(host.to_string()).or_insert(0) += 1;
        *agg.paths.entry(uri.to_string()).or_insert(0) += 1;
        if !ua.is_empty() {
            let ua_short = if ua.len() > 80 { &ua[..80] } else { ua };
            *agg.uas.entry(ua_short.to_string()).or_insert(0) += 1;
        }
        if !referer.is_empty() {
            *agg.refs.entry(referer.to_string()).or_insert(0) += 1;
        }
    }

    if agg.total == 0 {
        return Ok(false);
    }

    agg.unique_ips = agg.ips.len();

    // Atomic write: temp file then rename
    let json = serde_json::to_string(&agg)?;
    let tmp_path = format!("{}.tmp", agg_path);
    std::fs::write(&tmp_path, &json)?;
    std::fs::rename(&tmp_path, &agg_path)?;

    debug!("Rolled up {}/{}: {} requests, {} IPs", date, hour, agg.total, agg.unique_ips);
    Ok(true)
}

// ── Daily rollup ───────────────────────────────────────────────────────────

fn rollup_daily(agg_dir: &str, date: &str) -> Result<bool> {
    let daily_path = format!("{}/daily/{}.json", agg_dir, date);
    if Path::new(&daily_path).exists() {
        return Ok(false);
    }

    let mut daily = DailyAgg {
        date: date.to_string(),
        ..Default::default()
    };

    let mut all_ips: HashMap<String, u64> = HashMap::new();
    let mut found_any = false;

    for h in 0..24 {
        let hh = format!("{:02}", h);
        let hp = format!("{}/hourly/{}_{}.json", agg_dir, date, hh);
        let data = match std::fs::read_to_string(&hp) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let hourly: HourlyAgg = match serde_json::from_str(&data) {
            Ok(h) => h,
            Err(_) => continue,
        };
        found_any = true;

        daily.total += hourly.total;
        daily.bots += hourly.bots;
        daily.humans += hourly.humans;
        daily.suspects += hourly.suspects;
        daily.hourly.insert(hh, hourly.total);

        for (k, v) in &hourly.ips {
            *all_ips.entry(k.clone()).or_insert(0) += v;
        }
        for (k, v) in &hourly.paths {
            *daily.paths.entry(k.clone()).or_insert(0) += v;
        }
        for (k, v) in &hourly.hosts {
            *daily.hosts.entry(k.clone()).or_insert(0) += v;
        }
        for (k, v) in &hourly.crawlers {
            *daily.crawlers.entry(k.clone()).or_insert(0) += v;
        }
        for (k, v) in &hourly.refs {
            *daily.refs.entry(k.clone()).or_insert(0) += v;
        }
    }

    if !found_any {
        return Ok(false);
    }

    daily.unique_ips = all_ips.len();
    daily.ips = all_ips;

    let json = serde_json::to_string(&daily)?;
    let tmp_path = format!("{}.tmp", daily_path);
    std::fs::write(&tmp_path, &json)?;
    std::fs::rename(&tmp_path, &daily_path)?;

    info!("Daily rollup {}: {} requests, {} IPs", date, daily.total, daily.unique_ips);
    Ok(true)
}

// ── IP enrichment ──────────────────────────────────────────────────────────

/// Load IP cache from disk.
fn load_ip_cache(path: &str) -> HashMap<String, IpInfo> {
    match std::fs::read_to_string(path) {
        Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

/// Save IP cache to disk.
fn save_ip_cache(path: &str, cache: &HashMap<String, IpInfo>) -> Result<()> {
    let json = serde_json::to_string(cache)?;
    let tmp = format!("{}.tmp", path);
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Look up an IP via ip-api.com (free, no key, 45 req/min).
async fn lookup_ip(client: &reqwest::Client, ip: &str) -> Option<IpInfo> {
    let url = format!("http://ip-api.com/json/{}?fields=status,country,city,isp,org,as,query", ip);
    let resp = client.get(&url)
        .timeout(Duration::from_secs(10))
        .send().await.ok()?;
    let json: serde_json::Value = resp.json().await.ok()?;

    if json.get("status")?.as_str()? != "success" {
        return None;
    }

    Some(IpInfo {
        asn: json.get("as").and_then(|v| v.as_str()).map(|s| s.to_string()),
        org: json.get("org").and_then(|v| v.as_str())
            .or_else(|| json.get("isp").and_then(|v| v.as_str()))
            .map(|s| s.to_string()),
        country: json.get("country").and_then(|v| v.as_str()).map(|s| s.to_string()),
        city: json.get("city").and_then(|v| v.as_str()).map(|s| s.to_string()),
        network: None,
    })
}

/// Batch-enrich IPs: look up top IPs by request count that aren't in cache.
async fn enrich_ips(agg_dir: &str, date: &str, cache: &mut HashMap<String, IpInfo>) -> Result<usize> {
    // Collect all IPs from today's hourly files
    let mut all_ips: HashMap<String, u64> = HashMap::new();
    for h in 0..24 {
        let hp = format!("{}/hourly/{}_{:02}.json", agg_dir, date, h);
        let data = match std::fs::read_to_string(&hp) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let hourly: HourlyAgg = match serde_json::from_str(&data) {
            Ok(h) => h,
            Err(_) => continue,
        };
        for (k, v) in hourly.ips {
            *all_ips.entry(k).or_insert(0) += v;
        }
    }

    // Sort by count descending, take top IPs not yet in cache
    let mut ip_list: Vec<(String, u64)> = all_ips.into_iter().collect();
    ip_list.sort_by(|a, b| b.1.cmp(&a.1));

    let unknown: Vec<String> = ip_list.iter()
        .filter(|(ip, _)| !cache.contains_key(ip) && !ip.is_empty())
        .take(100)
        .map(|(ip, _)| ip.clone())
        .collect();

    if unknown.is_empty() {
        return Ok(0);
    }

    let client = reqwest::Client::new();
    let mut looked_up = 0;

    for ip in &unknown {
        match lookup_ip(&client, ip).await {
            Some(info) => {
                cache.insert(ip.clone(), info);
                looked_up += 1;
            }
            None => {
                // Store empty entry to avoid re-lookup
                cache.insert(ip.clone(), IpInfo::default());
            }
        }
        // Rate limit: ip-api.com allows 45/min, we do ~1.5/sec
        tokio::time::sleep(Duration::from_millis(1500)).await;

        // Cap per cycle
        if looked_up >= 40 {
            break;
        }
    }

    Ok(looked_up)
}

/// Write per-day enrichment file merging cache with today's IPs.
fn write_day_enrichment(agg_dir: &str, date: &str, cache: &HashMap<String, IpInfo>) -> Result<()> {
    // Collect today's IPs from hourly files
    let mut day_ips: HashMap<String, u64> = HashMap::new();
    for h in 0..24 {
        let hp = format!("{}/hourly/{}_{:02}.json", agg_dir, date, h);
        let data = match std::fs::read_to_string(&hp) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let hourly: HourlyAgg = match serde_json::from_str(&data) {
            Ok(h) => h,
            Err(_) => continue,
        };
        for (k, v) in hourly.ips {
            *day_ips.entry(k).or_insert(0) += v;
        }
    }

    // Build enrichment map for today's IPs only
    let mut enrichment: HashMap<String, IpInfo> = HashMap::new();
    for ip in day_ips.keys() {
        if let Some(info) = cache.get(ip) {
            if info.org.is_some() || info.country.is_some() {
                enrichment.insert(ip.clone(), info.clone());
            }
        }
    }

    let enrich_dir = format!("{}/ip_enrichment", agg_dir);
    let _ = std::fs::create_dir_all(&enrich_dir);
    let enrich_path = format!("{}/{}.json", enrich_dir, date);
    let json = serde_json::to_string(&enrichment)?;
    let tmp = format!("{}.tmp", enrich_path);
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, &enrich_path)?;
    Ok(())
}

// ── Background tasks ───────────────────────────────────────────────────────

/// Background task: aggregate raw crawl logs into hourly/daily JSON every 5 minutes.
pub async fn crawl_rollup_task(log_dir: String) {
    let crawl_dir = format!("{}/crawl", log_dir);
    let agg_dir = format!("{}/crawl_agg", log_dir);

    // Ensure directories exist
    let _ = std::fs::create_dir_all(format!("{}/hourly", agg_dir));
    let _ = std::fs::create_dir_all(format!("{}/daily", agg_dir));

    let mut tick = interval(Duration::from_secs(300));
    // Run immediately on first tick, then every 5 min
    loop {
        tick.tick().await;

        let now = Local::now();
        let today = now.format("%Y-%m-%d").to_string();
        let current_hour = now.format("%H").to_string().parse::<u32>().unwrap_or(0);

        // Rollup completed hours of today
        for h in 0..current_hour {
            let hh = format!("{:02}", h);
            match tokio::task::spawn_blocking({
                let crawl_dir = crawl_dir.clone();
                let agg_dir = agg_dir.clone();
                let today = today.clone();
                move || rollup_hour(&crawl_dir, &agg_dir, &today, &hh)
            }).await {
                Ok(Ok(true)) => {}
                Ok(Ok(false)) => {} // already done or no data
                Ok(Err(e)) => warn!("Rollup {}/{:02}: {}", today, h, e),
                Err(e) => warn!("Rollup task join error: {}", e),
            }
        }

        // Yesterday: all hours + daily
        let yesterday = (now - chrono::Duration::days(1)).format("%Y-%m-%d").to_string();
        for h in 0..24u32 {
            let hh = format!("{:02}", h);
            let cd = crawl_dir.clone();
            let ad = agg_dir.clone();
            let yd = yesterday.clone();
            let _ = tokio::task::spawn_blocking(move || rollup_hour(&cd, &ad, &yd, &hh)).await;
        }
        {
            let ad = agg_dir.clone();
            let yd = yesterday.clone();
            let _ = tokio::task::spawn_blocking(move || rollup_daily(&ad, &yd)).await;
        }

        info!("Crawl rollup cycle complete for {}", today);
    }
}

/// Background task: enrich IPs with geo/org data every 15 minutes.
pub async fn ip_enrichment_task(log_dir: String) {
    let agg_dir = format!("{}/crawl_agg", log_dir);
    let cache_path = format!("{}/ip_cache.json", agg_dir);

    // Wait 60s before first run to let rollup produce some data
    tokio::time::sleep(Duration::from_secs(60)).await;

    let mut tick = interval(Duration::from_secs(900)); // 15 min
    loop {
        tick.tick().await;

        let now = Local::now();
        let today = now.format("%Y-%m-%d").to_string();

        let mut cache = load_ip_cache(&cache_path);
        let cache_size_before = cache.len();

        match enrich_ips(&agg_dir, &today, &mut cache).await {
            Ok(n) if n > 0 => {
                info!("IP enrichment: looked up {} new IPs (cache: {} -> {})",
                    n, cache_size_before, cache.len());
                if let Err(e) = save_ip_cache(&cache_path, &cache) {
                    warn!("Failed to save IP cache: {}", e);
                }
            }
            Ok(_) => debug!("IP enrichment: no new IPs to look up"),
            Err(e) => warn!("IP enrichment error: {}", e),
        }

        // Write per-day enrichment file
        if let Err(e) = write_day_enrichment(&agg_dir, &today, &cache) {
            warn!("Failed to write day enrichment: {}", e);
        }
    }
}
