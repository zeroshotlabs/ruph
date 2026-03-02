//! INI-based configuration for ruph
//!
//! Loads settings from a .ini file with sections: [server], [php], [ssl]
//! CLI arguments override config file values.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use anyhow::{Result, anyhow};
use configparser::ini::Ini;
use tracing::info;

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
    // [server]
    pub bind: String,
    pub log_level: String,
    pub tls: bool,

    // [http]
    pub docroot: Option<String>,
    /// Per-domain docroot overrides: domain -> path.
    /// Populated by `docroot.<domain> = /path` keys in [http].
    pub domain_roots: HashMap<String, String>,
    pub index_files: Vec<String>,

    // [php]
    pub php_mode: PhpMode,
    pub php_binary: Option<String>,

    // [ssl]
    pub ssl_dir: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            bind: "0.0.0.0:8082".to_string(),
            log_level: "info".to_string(),
            tls: false,
            docroot: None,
            domain_roots: HashMap::new(),
            index_files: vec!["_index.php".to_string()],
            php_mode: PhpMode::Auto,
            php_binary: None,
            ssl_dir: None,
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

        info!("Loaded config from {}", path.display());

        let mut config = Config::default();

        // [server]
        if let Some(v) = ini.get("server", "bind") {
            config.bind = v;
        }
        // support "listen" + "port" as aliases (from proxee INI style)
        if let (Some(listen), Some(port)) = (ini.get("server", "listen"), ini.get("server", "port")) {
            if !listen.is_empty() {
                config.bind = format!("{}:{}", listen, port);
            }
        }
        if let Some(v) = ini.get("server", "log_level") {
            config.log_level = v;
        }
        if let Some(v) = ini.get("server", "tls") {
            config.tls = parse_bool(&v);
        }

        // [http]
        if let Some(v) = ini.get("http", "docroot") {
            if !v.is_empty() {
                config.docroot = Some(v);
            }
        }
        if let Some(v) = ini.get("http", "index_files") {
            config.index_files = v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        }
        // Per-domain docroots: any key of the form `docroot.<domain>` in [http].
        if let Some(http_section) = ini.get_map().and_then(|m| m.get("http").cloned()) {
            for (key, value) in &http_section {
                if let Some(domain) = key.strip_prefix("docroot.") {
                    if let Some(path) = value {
                        let path = path.trim();
                        if !path.is_empty() && !domain.is_empty() {
                            config.domain_roots.insert(domain.to_string(), path.to_string());
                        }
                    }
                }
            }
        }

        // [php]
        if let Some(v) = ini.get("php", "processor") {
            if v.trim().eq_ignore_ascii_case("libphp") {
                return Err(anyhow!(
                    "php.processor=libphp is not implemented. Use php.processor=cgi and set php.binary to a php-cgi binary."
                ));
            }
            config.php_mode = PhpMode::from_str(&v);
        }
        if let Some(v) = ini.get("php", "binary") {
            if !v.is_empty() {
                config.php_binary = Some(v);
            }
        }

        // [ssl]
        if let Some(v) = ini.get("ssl", "dir") {
            if !v.is_empty() {
                config.ssl_dir = Some(v);
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
