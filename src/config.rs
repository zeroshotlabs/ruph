//! Section-based INI configuration for ruph
//!
//! Supports sections: [server], [server.https], [server.http], [php.*],
//! [http.*], [https.<domain>], [ssl]
//! CLI arguments override config file values.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use anyhow::{Result, anyhow};
use configparser::ini::Ini;

/// PHP processor selection mode
#[derive(Debug, Clone, PartialEq)]
pub enum PhpMode {
    /// AST -> embedded -> cgi (default)
    Auto,
    /// External PHP binary first, then AST -> embedded
    Cgi,
    /// AST only (no fallback to external)
    Ast,
    /// Embedded regex processor only
    Embedded,
}

impl PhpMode {
    fn from_str(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "cgi" | "external" | "php" => PhpMode::Cgi,
            "ast" => PhpMode::Ast,
            "embedded" | "regex" => PhpMode::Embedded,
            _ => PhpMode::Auto,
        }
    }
}

/// Parsed configuration
#[derive(Debug, Clone)]
pub struct Config {
    // [server] globals
    pub bind: String,
    pub log_level: String,
    pub tls: bool,
    pub http_bind: Option<String>,
    pub log_console: bool,

    // Docroots
    pub docroot: Option<String>,
    pub http_docroot: Option<String>,
    /// Exact domain -> docroot (patterns with dots)
    pub domain_roots: HashMap<String, String>,
    /// Prefix -> docroot (patterns without dots, e.g. "www" matches "www.*")
    pub prefix_roots: Vec<(String, String)>,

    // Index files
    pub index_files: Vec<String>,

    // Logging (request logs)
    pub default_log: Option<String>,
    /// Exact domain -> log file
    pub domain_logs: HashMap<String, String>,
    /// Prefix -> log file
    pub prefix_logs: Vec<(String, String)>,

    // Error logs (PHP error_log(), AST warnings, etc.)
    pub default_error_log: Option<String>,
    /// Exact domain -> error log file
    pub domain_error_logs: HashMap<String, String>,
    /// Prefix -> error log file
    pub prefix_error_logs: Vec<(String, String)>,

    // [php]
    pub php_mode: PhpMode,
    pub php_binary: Option<String>,

    // [ssl]
    pub ssl_dir: Option<String>,

    // Status page
    pub status_page: Option<String>,

    // Rate limit window in seconds (default 2)
    pub rate_window: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            bind: "0.0.0.0:8082".to_string(),
            log_level: "info".to_string(),
            tls: false,
            http_bind: None,
            log_console: false,
            docroot: None,
            http_docroot: None,
            domain_roots: HashMap::new(),
            prefix_roots: Vec::new(),
            index_files: vec!["_index.php".to_string(), "index.html".to_string(), "index.htm".to_string()],
            default_log: None,
            domain_logs: HashMap::new(),
            prefix_logs: Vec::new(),
            default_error_log: None,
            domain_error_logs: HashMap::new(),
            prefix_error_logs: Vec::new(),
            php_mode: PhpMode::Auto,
            php_binary: None,
            ssl_dir: None,
            status_page: None,
            rate_window: 2,
        }
    }
}

impl Config {
    /// Load config from an INI file, returning defaults for missing values
    pub fn load(path: &Path) -> Result<Self> {
        let mut ini = Ini::new();
        ini.set_comment_symbols(&[';', '#']);
        ini.load(path.to_str().ok_or_else(|| anyhow!("Invalid config path"))?)
            .map_err(|e| anyhow!("Failed to read config {}: {}", path.display(), e))?;

        eprintln!("  config: {}", path.display());

        let mut config = Config::default();

        // ── [server] globals ──
        if let Some(v) = ini.get("server", "log_level") {
            config.log_level = v;
        }
        if let Some(v) = ini.get("server", "log_console") {
            config.log_console = parse_bool(&v);
        }
        if let Some(v) = ini.get("server", "logs") {
            let v = v.trim().to_string();
            if !v.is_empty() {
                config.default_log = Some(v);
            }
        }
        if let Some(v) = ini.get("server", "error_log") {
            let v = v.trim().to_string();
            if !v.is_empty() {
                config.default_error_log = Some(v);
            }
        }
        if let Some(v) = ini.get("server", "index_files") {
            config.index_files = v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        }
        if let Some(v) = ini.get("server", "docroot") {
            if !v.is_empty() {
                config.docroot = Some(v);
            }
        }
        if let Some(v) = ini.get("server", "status_page") {
            let v = v.trim().to_string();
            if !v.is_empty() {
                config.status_page = Some(if v.starts_with('/') { v } else { format!("/{}", v) });
            }
        }
        if let Some(v) = ini.get("server", "rate_window") {
            if let Ok(n) = v.trim().parse::<u64>() {
                config.rate_window = n;
            }
        }

        // ── [server.https] ──
        if let Some(v) = ini.get("server.https", "bind") {
            config.bind = v;
            config.tls = true; // HTTPS implies TLS by default
        }
        if let Some(v) = ini.get("server.https", "tls") {
            config.tls = parse_bool(&v);
        }

        // ── [server.http] ──
        if let Some(v) = ini.get("server.http", "bind") {
            let v = v.trim().to_string();
            if !v.is_empty() {
                config.http_bind = Some(v);
            }
        }

        // ── Backward compat: [server] bind/tls if [server.https] absent ──
        if ini.get("server.https", "bind").is_none() {
            if let Some(v) = ini.get("server", "bind") {
                config.bind = v;
            }
            // support "listen" + "port" aliases (proxee INI style)
            if let (Some(listen), Some(port)) = (ini.get("server", "listen"), ini.get("server", "port")) {
                if !listen.is_empty() {
                    config.bind = format!("{}:{}", listen, port);
                }
            }
            if let Some(v) = ini.get("server", "tls") {
                config.tls = parse_bool(&v);
            }
        }
        // Backward compat: [server] http_bind if [server.http] absent
        if ini.get("server.http", "bind").is_none() {
            if let Some(v) = ini.get("server", "http_bind") {
                let v = v.trim().to_string();
                if !v.is_empty() {
                    config.http_bind = Some(v);
                }
            }
        }

        // ── [php.*] or [php] ──
        let php_section = if ini.get("php.*", "processor").is_some() { "php.*" } else { "php" };
        if let Some(v) = ini.get(php_section, "processor") {
            if v.trim().eq_ignore_ascii_case("libphp") {
                return Err(anyhow!(
                    "php.processor=libphp is not implemented. Use php.processor=cgi and set php.binary to a php-cgi binary."
                ));
            }
            config.php_mode = PhpMode::from_str(&v);
        }
        if let Some(v) = ini.get(php_section, "binary") {
            if !v.is_empty() {
                config.php_binary = Some(v);
            }
        }

        // ── [ssl] ──
        if let Some(v) = ini.get("ssl", "dir") {
            if !v.is_empty() {
                config.ssl_dir = Some(v);
            }
        }

        // ── Virtual host sections: [http.*], [https.*], [https.<domain>], etc. ──
        if let Some(sections) = ini.get_map() {
            for (section_name, _keys) in &sections {
                // [http.*] — default HTTP docroot
                if section_name == "http.*" {
                    if let Some(v) = ini.get(section_name, "docroot") {
                        let v = v.trim().to_string();
                        if !v.is_empty() {
                            config.http_docroot = Some(v);
                        }
                    }
                    continue;
                }

                // [https.*] — default HTTPS docroot
                if section_name == "https.*" {
                    if let Some(v) = ini.get(section_name, "docroot") {
                        let v = v.trim().to_string();
                        if !v.is_empty() && config.docroot.is_none() {
                            config.docroot = Some(v);
                        }
                    }
                    continue;
                }

                // Sections starting with "https." — per-domain virtual hosts
                // Supports comma-separated: [https.a.com,https.b.com]
                if section_name.starts_with("https.") || section_name.contains(",https.") {
                    let docroot = ini.get(section_name, "docroot");
                    let logs = ini.get(section_name, "logs");
                    let error_log = ini.get(section_name, "error_log");

                    // Split comma-separated section names
                    for part in section_name.split(',') {
                        let part = part.trim();
                        if let Some(pattern) = part.strip_prefix("https.") {
                            if pattern.is_empty() || pattern == "*" {
                                continue;
                            }
                            let has_dot = pattern.contains('.');
                            if let Some(ref v) = docroot {
                                let v = v.trim();
                                if !v.is_empty() {
                                    if has_dot {
                                        config.domain_roots.insert(pattern.to_string(), v.to_string());
                                    } else {
                                        config.prefix_roots.push((pattern.to_string(), v.to_string()));
                                    }
                                }
                            }
                            if let Some(ref v) = logs {
                                let v = v.trim();
                                if !v.is_empty() {
                                    if has_dot {
                                        config.domain_logs.insert(pattern.to_string(), v.to_string());
                                    } else {
                                        config.prefix_logs.push((pattern.to_string(), v.to_string()));
                                    }
                                }
                            }
                            if let Some(ref v) = error_log {
                                let v = v.trim();
                                if !v.is_empty() {
                                    if has_dot {
                                        config.domain_error_logs.insert(pattern.to_string(), v.to_string());
                                    } else {
                                        config.prefix_error_logs.push((pattern.to_string(), v.to_string()));
                                    }
                                }
                            }
                        }
                    }
                    continue;
                }

                // [http.<domain>] sections (same logic, for future HTTP per-domain)
                if section_name.starts_with("http.") && section_name != "http.*" {
                    // HTTP per-domain not yet used, but parse for forward compat
                    continue;
                }
            }
        }

        // ── Backward compat: old-style [http] section with docroot.<domain> keys ──
        if config.domain_roots.is_empty() {
            if let Some(http_section) = ini.get_map().and_then(|m| m.get("http").cloned()) {
                for (key, value) in &http_section {
                    if let Some(domain) = key.strip_prefix("docroot.") {
                        if let Some(path) = value {
                            let path = path.trim();
                            if !path.is_empty() && !domain.is_empty() {
                                config.domain_roots.insert(domain.to_string(), path.to_string());
                            }
                        }
                    } else if let Some(domain) = key.strip_prefix("logs.") {
                        if let Some(path) = value {
                            let path = path.trim();
                            if !path.is_empty() && !domain.is_empty() {
                                config.domain_logs.insert(domain.to_string(), path.to_string());
                            }
                        }
                    } else if let Some(domain) = key.strip_prefix("error_log.") {
                        if let Some(path) = value {
                            let path = path.trim();
                            if !path.is_empty() && !domain.is_empty() {
                                config.domain_error_logs.insert(domain.to_string(), path.to_string());
                            }
                        }
                    }
                }
            }
            // Old-style [http] docroot / http_docroot / index_files / logs / error_log
            if config.docroot.is_none() {
                if let Some(v) = ini.get("http", "docroot") {
                    if !v.is_empty() { config.docroot = Some(v); }
                }
            }
            if config.http_docroot.is_none() {
                if let Some(v) = ini.get("http", "http_docroot") {
                    let v = v.trim().to_string();
                    if !v.is_empty() { config.http_docroot = Some(v); }
                }
            }
            if ini.get("server", "index_files").is_none() {
                if let Some(v) = ini.get("http", "index_files") {
                    config.index_files = v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                }
            }
            if config.default_log.is_none() {
                if let Some(v) = ini.get("http", "logs") {
                    let v = v.trim().to_string();
                    if !v.is_empty() { config.default_log = Some(v); }
                }
            }
            if config.default_error_log.is_none() {
                if let Some(v) = ini.get("http", "error_log") {
                    let v = v.trim().to_string();
                    if !v.is_empty() { config.default_error_log = Some(v); }
                }
            }
        }

        Ok(config)
    }

    /// Search for a config file in standard locations
    pub fn find_config(docroot: Option<&str>) -> Option<PathBuf> {
        let candidates: Vec<PathBuf> = vec![
            // Explicit docroot location
            docroot.map(|d| PathBuf::from(d).join("ruph.ini")),
            // Current directory
            Some(PathBuf::from("ruph.ini")),
            // Home directory
            std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".ruph").join("ruph.ini")),
            // /etc
            Some(PathBuf::from("/etc/ruph/ruph.ini")),
        ].into_iter().flatten().collect();

        for path in candidates {
            if path.exists() {
                return Some(path);
            }
        }
        None
    }
}

fn parse_bool(s: &str) -> bool {
    matches!(s.trim().to_lowercase().as_str(), "true" | "yes" | "on" | "1")
}
