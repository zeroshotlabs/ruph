use std::path::{Path, PathBuf};
use anyhow::{Result, anyhow};
use tracing::{info, warn};
use instant_acme::{Account, AccountCredentials, NewAccount, OrderStatus, AuthorizationStatus, ChallengeType, DirectoryUrl};
use rcgen::{CertificateParams, KeyPair};
use serde_json;
use tokio::net::TcpListener;
use hyper::{Request, Response, StatusCode};
use hyper::service::service_fn;
use hyper::server::conn::http1;
use hyper::body::Incoming as IncomingBody;
use http_body_util::Full;
use bytes::Bytes;
use hyper_util::rt::TokioIo;

pub async fn issue_cert(email: &str, domain: &str, ssl_dir: &Path) -> Result<()> {
    let domain_dir = ssl_dir.join(domain);
    std::fs::create_dir_all(&domain_dir)?;

    let account = create_or_load_account(email, ssl_dir).await?;
    let mut order = account.new_order(domain).await?;

    let auths = order.authorizations().await?;
    let auth = auths.get(0).ok_or_else(|| anyhow!("No authorization"))?;

    let challenge = auth.get_challenge(ChallengeType::Http01)
        .ok_or_else(|| anyhow!("HTTP-01 challenge not offered"))?;

    let token = challenge.token.clone();
    let key_auth = challenge.key_authorization(&account).await?;

    info!("Starting temporary HTTP-01 challenge server on :80 for {}", domain);
    let handle = tokio::spawn(run_challenge_server(token.clone(), key_auth.clone()));

    challenge.validate().await?;

    loop {
        let status = auth.refresh().await?.status;
        if status == AuthorizationStatus::Valid {
            break;
        }
        if status == AuthorizationStatus::Invalid {
            return Err(anyhow!("ACME challenge failed"));
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    handle.abort();

    let keypair = KeyPair::generate()?;
    let mut params = CertificateParams::new(vec![domain.to_string()]);
    params.key_pair = Some(keypair.clone());
    let csr = params.serialize_request_pem()?;

    order.finalize(csr).await?;

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

    let cert_chain = order.download_certificate().await?;

    std::fs::write(domain_dir.join("fullchain.pem"), cert_chain)?;
    std::fs::write(domain_dir.join("privkey.pem"), keypair.serialize_pem())?;

    info!("Certificate issued for {} and saved to {}", domain, domain_dir.display());

    Ok(())
}

async fn create_or_load_account(email: &str, ssl_dir: &Path) -> Result<Account> {
    let creds_path = ssl_dir.join("acme_account.json");
    if creds_path.exists() {
        let data = std::fs::read_to_string(&creds_path)?;
        let creds: AccountCredentials = serde_json::from_str(&data)?;
        return Ok(Account::from_credentials(creds, DirectoryUrl::LetsEncrypt).await?);
    }

    let new_account = NewAccount {
        contact: vec![format!("mailto:{}", email)],
        terms_of_service_agreed: true,
        only_return_existing: false,
    };

    let account = Account::create(&new_account, DirectoryUrl::LetsEncrypt).await?;
    let creds = account.credentials().await?;
    std::fs::write(&creds_path, serde_json::to_string_pretty(&creds)?)?;
    Ok(account)
}

async fn run_challenge_server(token: String, key_auth: String) -> Result<()> {
    let listener = TcpListener::bind("0.0.0.0:80").await?;
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
