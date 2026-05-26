use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Persistent mapping from base domain → stable UUID subdomain.
///
/// Stored at `~/.ruph/ssl/acme_domains.json`.
/// Wildcards share the same UUID as their bare domain since the CNAME
/// target (`_acme-challenge.<base>`) is identical.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct AcmeDomainMap {
    pub domains: HashMap<String, String>,
}

impl AcmeDomainMap {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let data = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&data)?)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let data = serde_json::to_string_pretty(self)?;
        std::fs::write(path, data)?;
        Ok(())
    }

    /// Get or create a stable UUID subdomain for the given domain.
    /// Strips `*.` prefix so wildcards and bare domains share the same CNAME.
    /// Returns `(uuid_subdomain, is_new)`.
    pub fn get_or_create(&mut self, domain: &str) -> (String, bool) {
        let base = base_domain(domain);
        if let Some(existing) = self.domains.get(&base) {
            (existing.clone(), false)
        } else {
            let uuid = uuid::Uuid::new_v4().to_string();
            self.domains.insert(base, uuid.clone());
            (uuid, true)
        }
    }

}

/// Strip `*.` prefix and lowercase to get the base domain for CNAME purposes.
pub fn base_domain(domain: &str) -> String {
    domain
        .trim_start_matches("*.")
        .to_lowercase()
}
