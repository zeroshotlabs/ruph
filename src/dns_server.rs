use anyhow::{anyhow, Result};
use simple_dns::rdata::{RData, TXT, SOA};
use simple_dns::{Name, Packet, PacketFlag, ResourceRecord, CLASS, RCODE, QTYPE, TYPE};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// Thread-safe mutable store of TXT records.
/// Key: lowercase FQDN (e.g., "abcd1234.auth.example.com")
/// Value: TXT record content (the ACME DNS-01 digest)
pub type TxtRecordStore = Arc<RwLock<HashMap<String, String>>>;

pub fn new_txt_store() -> TxtRecordStore {
    Arc::new(RwLock::new(HashMap::new()))
}

pub struct DnsServerConfig {
    /// The zone this server is authoritative for (e.g., "auth.example.com")
    pub zone: String,
    /// UDP bind address (default "0.0.0.0:53")
    pub bind: String,
}

/// Run the embedded DNS server. Listens on UDP, answers TXT queries for
/// subdomains of the configured zone. Runs forever until the task is cancelled.
pub async fn run_dns_server(config: DnsServerConfig, records: TxtRecordStore) -> Result<()> {
    let socket = UdpSocket::bind(&config.bind).await.map_err(|e| {
        anyhow!(
            "Cannot bind DNS server to {} ({}).\n\
             UDP port 53 must be available. On Linux, use: setcap cap_net_bind_service=+ep /path/to/ruph",
            config.bind,
            e
        )
    })?;
    info!("DNS server listening on {} for zone {}", config.bind, config.zone);

    let mut buf = vec![0u8; 512];
    loop {
        let (len, src) = match socket.recv_from(&mut buf).await {
            Ok(r) => r,
            Err(e) => {
                warn!("DNS recv error: {}", e);
                continue;
            }
        };

        let records_snapshot = records.read().await;
        match handle_dns_query(&buf[..len], &config.zone, &records_snapshot) {
            Some(response) => {
                if let Err(e) = socket.send_to(&response, src).await {
                    warn!("DNS send error: {}", e);
                }
            }
            None => {
                debug!("DNS: dropped malformed packet from {}", src);
            }
        }
    }
}

/// Process a single DNS query and produce a response.
/// Returns None for malformed packets.
fn handle_dns_query(
    query_bytes: &[u8],
    zone: &str,
    records: &HashMap<String, String>,
) -> Option<Vec<u8>> {
    let query = Packet::parse(query_bytes).ok()?;

    // Must be a query with exactly one question
    if query.has_flags(PacketFlag::RESPONSE) || query.questions.len() != 1 {
        return None;
    }

    // Extract question fields before consuming query
    let qname = query.questions[0].qname.to_string().to_lowercase();
    let qtype = query.questions[0].qtype;
    // Normalize: remove trailing dot
    let qname_clean = qname.trim_end_matches('.');
    let zone_lower = zone.to_lowercase();
    let zone_clean = zone_lower.trim_end_matches('.');

    let mut reply = query.into_reply();
    reply.set_flags(PacketFlag::AUTHORITATIVE_ANSWER);

    // Check if the query is within our zone
    let in_zone = qname_clean == zone_clean || qname_clean.ends_with(&format!(".{}", zone_clean));

    if !in_zone {
        // Not our zone — REFUSED
        *reply.rcode_mut() = RCODE::Refused;
        return reply.build_bytes_vec_compressed().ok();
    }

    match qtype {
        QTYPE::TYPE(TYPE::TXT) => {
            // Look up TXT record
            if let Some(txt_value) = records.get(qname_clean) {
                let name = Name::new_unchecked(&qname_clean);
                let mut txt = TXT::new();
                let _ = txt.add_string(txt_value);
                let rr = ResourceRecord::new(name, CLASS::IN, 1, RData::TXT(txt));
                reply.answers.push(rr);
                *reply.rcode_mut() = RCODE::NoError;
                debug!("DNS: TXT hit for {} -> {}...", qname_clean, &txt_value[..txt_value.len().min(20)]);
            } else {
                *reply.rcode_mut() = RCODE::NameError;
                // Add SOA to authority section for proper NXDOMAIN
                if let Some(soa_rr) = build_soa(zone_clean) {
                    reply.name_servers.push(soa_rr);
                }
            }
        }
        QTYPE::TYPE(TYPE::SOA) if qname_clean == zone_clean => {
            if let Some(soa_rr) = build_soa(zone_clean) {
                reply.answers.push(soa_rr);
            }
            *reply.rcode_mut() = RCODE::NoError;
        }
        _ => {
            // We only serve TXT records; return empty answer (not REFUSED, since it's our zone)
            *reply.rcode_mut() = RCODE::NoError;
        }
    }

    reply.build_bytes_vec_compressed().ok()
}

/// Build a minimal SOA record for the zone.
fn build_soa(zone: &str) -> Option<ResourceRecord<'static>> {
    let name = Name::new_unchecked(zone).into_owned();
    let mname = Name::new_unchecked(zone).into_owned();
    let rname = Name::new_unchecked(&format!("hostmaster.{}", zone)).into_owned();
    let soa = SOA {
        mname,
        rname,
        serial: 1,
        refresh: 3600,
        retry: 900,
        expire: 604800,
        minimum: 1,
    };
    Some(ResourceRecord::new(name, CLASS::IN, 300, RData::SOA(soa)))
}

/// Query a public DNS resolver for the CNAME of `_acme-challenge.<base_domain>`
/// and verify it points to `expected_target`.
/// Uses `dig` subprocess for reliability (avoids conflicts with local UDP :53 listener).
pub async fn verify_cname(base_domain: &str, expected_target: &str) -> bool {
    let qname = format!("_acme-challenge.{}", base_domain);
    let expected_lower = expected_target.to_lowercase();
    let expected_clean = expected_lower.trim_end_matches('.');

    for resolver in &["8.8.8.8", "1.1.1.1"] {
        let output = tokio::process::Command::new("dig")
            .args(["+short", "CNAME", &qname, &format!("@{}", resolver)])
            .output()
            .await;

        if let Ok(out) = output {
            let stdout = String::from_utf8_lossy(&out.stdout);
            for line in stdout.lines() {
                let target = line.trim().trim_end_matches('.').to_lowercase();
                if target == expected_clean {
                    return true;
                }
            }
        }
    }

    false
}
