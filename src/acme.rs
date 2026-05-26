use anyhow::{anyhow, Result};
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming as IncomingBody;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use instant_acme::{
    Account, AccountCredentials, ChallengeType, Identifier, LetsEncrypt, NewAccount, NewOrder,
    OrderStatus,
};
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};
use serde_json;
use std::path::Path;
use tokio::net::TcpListener;
use tracing::info;

use crate::acme_state::{self, AcmeDomainMap};
use crate::dns_server::{self, DnsServerConfig, TxtRecordStore};

/// Issue a certificate. Dispatches to DNS-01 (when acme_zone is configured) or HTTP-01.
pub async fn issue_cert(
    email: &str,
    domain: &str,
    ssl_dir: &Path,
    acme_zone: Option<&str>,
    txt_records: Option<TxtRecordStore>,
) -> Result<()> {
    if let Some(zone) = acme_zone {
        issue_cert_dns01(email, domain, ssl_dir, zone, txt_records).await
    } else {
        issue_cert_http01(email, domain, ssl_dir).await
    }
}

/// Issue a certificate using DNS-01 challenge via the embedded DNS server.
/// Supports wildcards (e.g. *.example.com).
async fn issue_cert_dns01(
    email: &str,
    domain: &str,
    ssl_dir: &Path,
    zone: &str,
    txt_records: Option<TxtRecordStore>,
) -> Result<()> {
    let base = acme_state::base_domain(domain);

    let domain_dir = ssl_dir.join(domain);
    std::fs::create_dir_all(&domain_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&domain_dir, std::fs::Permissions::from_mode(0o700));
    }

    let account = create_or_load_account(email, ssl_dir).await?;

    // Load or create stable UUID subdomain for this domain
    let map_path = ssl_dir.join("acme_domains.json");
    let mut domain_map = AcmeDomainMap::load(&map_path)?;
    let (uuid_sub, is_new) = domain_map.get_or_create(domain);
    let acme_fqdn = format!("{}.{}", uuid_sub, zone);
    let cname_source = format!("_acme-challenge.{}", base);

    if is_new {
        domain_map.save(&map_path)?;
    }

    eprintln!();
    eprintln!("DNS-01 challenge mode (zone: {})", zone);
    if is_new {
        eprintln!();
        eprintln!("Add this DNS record (one-time, at whatever provider manages {}):", base);
        eprintln!();
        eprintln!("  {}  CNAME  {}", cname_source, acme_fqdn);
        eprintln!();
    } else {
        eprintln!("Using existing CNAME mapping for {}", base);
        eprintln!("  {}  CNAME  {}", cname_source, acme_fqdn);
    }

    // Start temporary DNS server if one isn't already running
    let (records, _dns_handle) = if let Some(existing) = txt_records {
        (existing, None)
    } else {
        let records = dns_server::new_txt_store();
        let dns_bind = "0.0.0.0:53".to_string();
        let zone_clone = zone.to_string();
        let records_clone = records.clone();
        let handle = tokio::spawn(async move {
            if let Err(e) = dns_server::run_dns_server(
                DnsServerConfig {
                    zone: zone_clone,
                    bind: dns_bind,
                },
                records_clone,
            )
            .await
            {
                eprintln!("DNS server error: {}", e);
            }
        });
        // Give the DNS server a moment to bind
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        (records, Some(handle))
    };

    // Verify CNAME propagation
    eprintln!("Verifying CNAME propagation...");
    let mut cname_ok = false;
    for attempt in 1..=60 {
        if dns_server::verify_cname(&base, &acme_fqdn).await {
            cname_ok = true;
            eprintln!("CNAME verified!");
            break;
        }
        if attempt == 1 {
            eprintln!("Waiting for CNAME propagation (checking every 10s, up to 10 min)...");
        }
        if attempt % 6 == 0 {
            eprintln!("  still waiting... ({}s elapsed)", attempt * 10);
        }
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    }
    if !cname_ok {
        return Err(anyhow!(
            "CNAME not found after 10 minutes.\n\
             Please ensure this DNS record exists:\n\n  \
             {}  CNAME  {}\n\n\
             Then re-run this command.",
            cname_source,
            acme_fqdn
        ));
    }

    // Create ACME order
    let identifiers = vec![Identifier::Dns(domain.to_string())];
    let new_order = NewOrder {
        identifiers: &identifiers,
    };
    let mut order = account.new_order(&new_order).await?;

    // Process each authorization (typically 1, but could be more for multi-SAN)
    let auths = order.authorizations().await?;
    for auth in &auths {
        let challenge = auth
            .challenges
            .iter()
            .find(|c| c.r#type == ChallengeType::Dns01)
            .ok_or_else(|| {
                anyhow!(
                    "DNS-01 challenge not offered for {}. Available: {:?}",
                    domain,
                    auth.challenges
                        .iter()
                        .map(|c| format!("{:?}", c.r#type))
                        .collect::<Vec<_>>()
                )
            })?;

        let key_auth = order.key_authorization(challenge);
        let dns_value = key_auth.dns_value();

        info!("Setting TXT record on {} for DNS-01 challenge", acme_fqdn);
        records
            .write()
            .await
            .insert(acme_fqdn.to_lowercase(), dns_value);

        // Small delay to ensure DNS server is serving the new record
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        order.set_challenge_ready(&challenge.url).await?;

        // Poll until this authorization passes
        eprintln!("Waiting for ACME validation...");
        loop {
            let status = order.refresh().await?.status;
            if status == OrderStatus::Ready || status == OrderStatus::Valid {
                break;
            }
            if status == OrderStatus::Invalid {
                // Clean up TXT record
                records.write().await.remove(&acme_fqdn.to_lowercase());
                return Err(anyhow!(
                    "ACME DNS-01 challenge failed for {}. \
                     Verify the CNAME record is correct and DNS is propagated.",
                    domain
                ));
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }

        // Clean up TXT record for this authorization
        records.write().await.remove(&acme_fqdn.to_lowercase());
    }

    // Generate key and CSR, finalize order
    finalize_and_save(&mut order, domain, &domain_dir).await?;

    // Abort temporary DNS server if we started one
    if let Some(handle) = _dns_handle {
        handle.abort();
    }

    info!(
        "Certificate issued for {} and saved to {}",
        domain,
        domain_dir.display()
    );

    Ok(())
}

/// Issue a certificate using HTTP-01 challenge (original flow).
/// Requires port 80 to be free. Does not support wildcards.
async fn issue_cert_http01(email: &str, domain: &str, ssl_dir: &Path) -> Result<()> {
    if domain.contains('*') {
        return Err(anyhow!(
            "Wildcard certificates (e.g. *.example.com) require DNS-01 validation.\n\
             Configure [acme] zone in ruph.ini to enable DNS-01, or use certbot:\n  \
             certbot certonly --dns-<provider> -d \"{}\" -d \"{}\"\n\
             Then copy fullchain.pem and privkey.pem to {}/",
            domain,
            domain.trim_start_matches("*."),
            ssl_dir.join(domain).display()
        ));
    }

    let domain_dir = ssl_dir.join(domain);
    std::fs::create_dir_all(&domain_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&domain_dir, std::fs::Permissions::from_mode(0o700));
    }

    let account = create_or_load_account(email, ssl_dir).await?;

    let identifiers = vec![Identifier::Dns(domain.to_string())];
    let order = NewOrder {
        identifiers: &identifiers,
    };
    let mut order = account.new_order(&order).await?;

    let auths = order.authorizations().await?;
    let auth = auths.get(0).ok_or_else(|| anyhow!("No authorization"))?;
    let challenge = auth
        .challenges
        .iter()
        .find(|c| c.r#type == ChallengeType::Http01)
        .ok_or_else(|| {
            anyhow!(
                "HTTP-01 challenge not offered for {}. Available: {:?}",
                domain,
                auth.challenges
                    .iter()
                    .map(|c| format!("{:?}", c.r#type))
                    .collect::<Vec<_>>()
            )
        })?;

    let token = challenge.token.clone();
    let key_auth = order.key_authorization(challenge).as_str().to_string();

    info!(
        "Starting temporary HTTP-01 challenge server on :80 for {}",
        domain
    );
    let challenge_listener = TcpListener::bind("0.0.0.0:80").await.map_err(|e| {
        anyhow!(
            "Cannot bind ACME challenge server to :80 ({}).\n\
             Port 80 must be free for HTTP-01 validation. Stop nginx/ruph first, \
             or configure [acme] zone in ruph.ini for DNS-01 validation.",
            e
        )
    })?;
    let handle = tokio::spawn(run_challenge_server(
        challenge_listener,
        token.clone(),
        key_auth.clone(),
    ));

    order.set_challenge_ready(&challenge.url).await?;

    loop {
        let status = order.refresh().await?.status;
        if status == OrderStatus::Ready {
            break;
        }
        if status == OrderStatus::Invalid {
            return Err(anyhow!("ACME challenge failed"));
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    handle.abort();

    finalize_and_save(&mut order, domain, &domain_dir).await?;

    info!(
        "Certificate issued for {} and saved to {}",
        domain,
        domain_dir.display()
    );

    Ok(())
}

/// Generate keypair, create CSR, finalize ACME order, and save cert + key to disk.
async fn finalize_and_save(
    order: &mut instant_acme::Order,
    domain: &str,
    domain_dir: &Path,
) -> Result<()> {
    let keypair = KeyPair::generate()?;

    // CSR SANs must match the ACME order identifiers exactly
    let mut params = CertificateParams::new(vec![domain.to_string()])?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, domain);
    params.distinguished_name = dn;
    let csr = params.serialize_request(&keypair)?;
    order.finalize(csr.der()).await?;

    loop {
        let status = order.refresh().await?.status;
        if status == OrderStatus::Valid {
            break;
        }
        if status == OrderStatus::Invalid {
            return Err(anyhow!("Order became invalid during finalization"));
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    let cert_chain = order
        .certificate()
        .await?
        .ok_or_else(|| anyhow!("No certificate returned"))?;

    std::fs::write(domain_dir.join("fullchain.pem"), cert_chain)?;
    let privkey_path = domain_dir.join("privkey.pem");
    std::fs::write(&privkey_path, keypair.serialize_pem())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&privkey_path, std::fs::Permissions::from_mode(0o600));
    }

    Ok(())
}

async fn create_or_load_account(email: &str, ssl_dir: &Path) -> Result<Account> {
    let creds_path = ssl_dir.join("acme_account.json");
    if creds_path.exists() {
        let data = std::fs::read_to_string(&creds_path)?;
        let creds: AccountCredentials = serde_json::from_str(&data)?;
        info!("Loading ACME account from {}", creds_path.display());
        return Ok(Account::from_credentials(creds).await?);
    }

    info!("Creating new ACME account for {}", email);
    let contact = format!("mailto:{}", email);
    let contacts = [contact.as_str()];
    let new_account = NewAccount {
        contact: &contacts,
        terms_of_service_agreed: true,
        only_return_existing: false,
    };

    let (account, creds) =
        Account::create(&new_account, LetsEncrypt::Production.url(), None).await?;
    std::fs::write(&creds_path, serde_json::to_string_pretty(&creds)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&creds_path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(account)
}

async fn run_challenge_server(
    listener: TcpListener,
    token: String,
    key_auth: String,
) -> Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let token = token.clone();
        let key_auth = key_auth.clone();
        tokio::task::spawn(async move {
            let io = TokioIo::new(stream);
            let service = service_fn(move |req: Request<IncomingBody>| {
                let token = token.clone();
                let key_auth = key_auth.clone();
                async move {
                    let path = req.uri().path();
                    let expected = format!("/.well-known/acme-challenge/{}", token);
                    if path == expected {
                        let body = Full::new(Bytes::from(key_auth));
                        Ok::<_, std::convert::Infallible>(
                            Response::builder()
                                .status(StatusCode::OK)
                                .body(body)
                                .unwrap(),
                        )
                    } else {
                        Ok::<_, std::convert::Infallible>(
                            Response::builder()
                                .status(StatusCode::NOT_FOUND)
                                .body(Full::new(Bytes::from("Not Found")))
                                .unwrap(),
                        )
                    }
                }
            });

            let _ = http1::Builder::new().serve_connection(io, service).await;
        });
    }
}
