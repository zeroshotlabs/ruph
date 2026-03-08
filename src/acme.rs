use std::path::Path;
use anyhow::{Result, anyhow};
use tracing::info;
use instant_acme::{Account, AccountCredentials, NewAccount, NewOrder, Identifier, LetsEncrypt, OrderStatus, ChallengeType};
use rcgen::{CertificateParams, KeyPair, DistinguishedName, DnType};
use tokio::net::TcpListener;
use hyper::{Request, Response, StatusCode};
use hyper::service::service_fn;
use hyper::server::conn::http1;
use hyper::body::Incoming as IncomingBody;
use http_body_util::Full;
use bytes::Bytes;
use hyper_util::rt::TokioIo;
use serde_json;

pub async fn issue_cert(email: &str, domain: &str, ssl_dir: &Path) -> Result<()> {
    if domain.contains('*') {
        return Err(anyhow!(
            "Wildcard certificates (e.g. *.example.com) require DNS-01 validation \
             which ruph does not support. Use certbot with a DNS plugin instead:\n  \
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
    let order = NewOrder { identifiers: &identifiers };
    let mut order = account.new_order(&order).await?;

    let auths = order.authorizations().await?;
    let auth = auths.get(0).ok_or_else(|| anyhow!("No authorization"))?;
    let challenge = auth.challenges.iter()
        .find(|c| c.r#type == ChallengeType::Http01)
        .ok_or_else(|| anyhow!("HTTP-01 challenge not offered for {}. Available: {:?}",
            domain,
            auth.challenges.iter().map(|c| format!("{:?}", c.r#type)).collect::<Vec<_>>()
        ))?;

    let token = challenge.token.clone();
    let key_auth = order.key_authorization(challenge).as_str().to_string();

    // Bind the challenge listener NOW so port conflicts fail immediately
    info!("Starting temporary HTTP-01 challenge server on :80 for {}", domain);
    let challenge_listener = TcpListener::bind("0.0.0.0:80").await
        .map_err(|e| anyhow!(
            "Cannot bind ACME challenge server to :80 ({}).\n\
             Port 80 must be free for HTTP-01 validation. Stop nginx/ruph first, or use certbot.",
            e
        ))?;
    let handle = tokio::spawn(run_challenge_server(challenge_listener, token.clone(), key_auth.clone()));

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

    let keypair = KeyPair::generate()?;
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
            return Err(anyhow!("Order became invalid"));
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    let cert_chain = order.certificate().await?
        .ok_or_else(|| anyhow!("No certificate returned"))?;

    std::fs::write(domain_dir.join("fullchain.pem"), cert_chain)?;
    let privkey_path = domain_dir.join("privkey.pem");
    std::fs::write(&privkey_path, keypair.serialize_pem())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&privkey_path, std::fs::Permissions::from_mode(0o600));
    }

    info!("Certificate issued for {} and saved to {}", domain, domain_dir.display());

    Ok(())
}

async fn create_or_load_account(email: &str, ssl_dir: &Path) -> Result<Account> {
    let creds_path = ssl_dir.join("acme_account.json");
    if creds_path.exists() {
        let data = std::fs::read_to_string(&creds_path)?;
        let creds: AccountCredentials = serde_json::from_str(&data)?;
        return Ok(Account::from_credentials(creds).await?);
    }

    let contact = format!("mailto:{}", email);
    let contacts = [contact.as_str()];
    let new_account = NewAccount {
        contact: &contacts,
        terms_of_service_agreed: true,
        only_return_existing: false,
    };

    let (account, creds) = Account::create(&new_account, LetsEncrypt::Production.url(), None).await?;
    std::fs::write(&creds_path, serde_json::to_string_pretty(&creds)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&creds_path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(account)
}

async fn run_challenge_server(listener: TcpListener, token: String, key_auth: String) -> Result<()> {
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
                        Ok::<_, std::convert::Infallible>(Response::builder()
                            .status(StatusCode::OK)
                            .body(body)
                            .unwrap())
                    } else {
                        Ok::<_, std::convert::Infallible>(Response::builder()
                            .status(StatusCode::NOT_FOUND)
                            .body(Full::new(Bytes::from("Not Found")))
                            .unwrap())
                    }
                }
            });

            let _ = http1::Builder::new().serve_connection(io, service).await;
        });
    }
}
