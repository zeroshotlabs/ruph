use std::fs::File;
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};
use anyhow::{Result, anyhow};
use rustls::{Certificate, PrivateKey, ServerConfig};
use rustls::server::ResolvesServerCertUsingSni;
use rustls::sign::{self, CertifiedKey};
use rustls_pemfile::{certs, pkcs8_private_keys, rsa_private_keys};
use tracing::warn;

pub fn default_ssl_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".ruph").join("ssl");
    }
    PathBuf::from(".ruph").join("ssl")
}

pub fn build_tls_config(ssl_dir: &Path) -> Result<ServerConfig> {
    let mut resolver = ResolvesServerCertUsingSni::new();

    if ssl_dir.exists() {
        for entry in std::fs::read_dir(ssl_dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if let Some(domain) = path.file_name().and_then(|s| s.to_str()) {
                if let Ok(cert_key) = load_cert_key(&path) {
                    resolver.add(domain.to_string(), cert_key).ok();
                }
            }
        }
    }

    if resolver.is_empty() {
        return Err(anyhow!("No TLS certificates found in {}", ssl_dir.display()));
    }

    let config = ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_cert_resolver(std::sync::Arc::new(resolver));

    Ok(config)
}

fn load_cert_key(domain_dir: &Path) -> Result<CertifiedKey> {
    let cert_path = domain_dir.join("fullchain.pem");
    let key_path = domain_dir.join("privkey.pem");

    let certs = read_certs(&cert_path)?;
    let key = read_key(&key_path)?;

    let signing_key = sign::any_supported_type(&key)
        .map_err(|_| anyhow!("Invalid private key"))?;

    Ok(CertifiedKey::new(certs, std::sync::Arc::new(signing_key)))
}

fn read_certs(path: &Path) -> Result<Vec<Certificate>> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let certs = certs(&mut reader)
        .collect::<io::Result<Vec<Vec<u8>>>>()?
        .into_iter()
        .map(Certificate)
        .collect();
    Ok(certs)
}

fn read_key(path: &Path) -> Result<PrivateKey> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut keys = pkcs8_private_keys(&mut reader)
        .collect::<io::Result<Vec<Vec<u8>>>>()?;
    if let Some(key) = keys.pop() {
        return Ok(PrivateKey(key));
    }

    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut keys = rsa_private_keys(&mut reader)
        .collect::<io::Result<Vec<Vec<u8>>>>()?;
    if let Some(key) = keys.pop() {
        return Ok(PrivateKey(key));
    }

    Err(anyhow!("No private keys found"))
}

pub fn warn_expiring(ssl_dir: &Path, days: i64) {
    if !ssl_dir.exists() {
        return;
    }

    let threshold = chrono::Utc::now() + chrono::Duration::days(days);

    for entry in std::fs::read_dir(ssl_dir).unwrap_or_else(|_| std::fs::read_dir(".").unwrap()) {
        if let Ok(entry) = entry {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let cert_path = path.join("fullchain.pem");
            if !cert_path.exists() {
                continue;
            }
            if let Ok(expiry) = cert_expiry(&cert_path) {
                if expiry <= threshold {
                    if let Some(domain) = path.file_name().and_then(|s| s.to_str()) {
                        warn!("TLS certificate for {} expires on {}", domain, expiry);
                    }
                }
            }
        }
    }
}

fn cert_expiry(path: &Path) -> Result<chrono::DateTime<chrono::Utc>> {
    let data = std::fs::read(path)?;
    let (_, pem) = x509_parser::pem::parse_x509_pem(&data)
        .map_err(|_| anyhow!("Failed to parse certificate"))?;
    let cert = pem.parse_x509().map_err(|_| anyhow!("Failed to parse x509"))?;
    let not_after = cert.validity().not_after.to_datetime();
    Ok(chrono::DateTime::<chrono::Utc>::from_utc(not_after, chrono::Utc))
}
