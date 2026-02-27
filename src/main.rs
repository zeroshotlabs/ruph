use anyhow::{Result, anyhow};
use clap::Parser;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{error, info, warn};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{body::Incoming as IncomingBody, Request, Response, StatusCode};
use http_body_util::Full;
use bytes::Bytes;
use std::convert::Infallible;
use tokio::net::TcpListener;
use hyper_util::rt::TokioIo;

mod ast_php_processor;
mod embedded_php_processor;
mod php_processor;
mod web_server;
mod config;
mod ssl;
mod acme;

use crate::web_server::WebServer;

type ResponseBody = Full<Bytes>;

#[derive(Parser, Debug)]
#[command(name = "ruph")]
#[command(about = "Rust + PHP-ish web server", long_about = None)]
struct Cli {
    /// Root directory to serve
    #[arg(value_name = "DOCROOT")]
    root: Option<String>,

    /// Bind address, e.g. 0.0.0.0:8082
    #[arg(long, default_value = "0.0.0.0:8082")]
    bind: String,

    /// Configuration file (INI format)
    #[arg(long, short = 'c', value_name = "FILE")]
    config: Option<String>,

    /// Generate a new TLS certificate: email@domain.com,example.com
    #[arg(long)]
    new_cert: Option<String>,

    /// List known certificates and exit
    #[arg(long, default_value_t = false)]
    list_certs: bool,

    /// Enable TLS (uses certs from ~/.ruph/ssl)
    #[arg(long, default_value_t = false)]
    tls: bool,

    /// Log level (error, warn, info, debug, trace)
    #[arg(long, default_value = "info")]
    log_level: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Show help and exit if invoked with no arguments at all
    if std::env::args().len() == 1 {
        Cli::parse_from(["ruph", "--help"]);
    }

    let cli = Cli::parse();

    // Load config: explicit --config, or auto-discover ruph.ini
    let cfg = if let Some(ref path) = cli.config {
        config::Config::load(std::path::Path::new(path))?
    } else if let Some(found) = config::Config::find_config(cli.root.as_deref()) {
        config::Config::load(&found)?
    } else {
        config::Config::default()
    };

    // CLI overrides config (CLI takes precedence)
    let log_level = if cli.log_level != "info" { &cli.log_level } else { &cfg.log_level };
    init_logging(log_level)?;

    if let Err(err) = rustls::crypto::ring::default_provider().install_default() {
        warn!("Failed to install rustls crypto provider: {:?}", err);
    }

    let ssl_dir = match &cfg.ssl_dir {
        Some(d) => std::path::PathBuf::from(d),
        None => ssl::default_ssl_dir(),
    };
    ssl::warn_expiring(&ssl_dir, 30);

    if cli.list_certs {
        let certs = ssl::list_certs(&ssl_dir)?;
        if certs.is_empty() {
            println!("No certificates found in {}", ssl_dir.display());
        } else {
            for (domain, expiry) in certs {
                println!("{}	{}", domain, expiry);
            }
        }
        return Ok(());
    }

    if let Some(spec) = &cli.new_cert {
        let parts: Vec<&str> = spec.split(",").collect();
        if parts.len() != 2 {
            return Err(anyhow!("--new-cert must be email@domain,example.com"));
        }
        let email = parts[0].trim();
        let domain = parts[1].trim();
        acme::issue_cert(email, domain, &ssl_dir).await?;
    }

    // Resolve docroot: CLI arg > config > error
    let root_dir = if let Some(ref r) = cli.root {
        std::path::PathBuf::from(r)
    } else if let Some(ref d) = cfg.docroot {
        std::path::PathBuf::from(d)
    } else if !cli.list_certs && cli.new_cert.is_none() {
        return Err(anyhow!("DOCROOT is required. Run with --help for usage."));
    } else {
        std::env::current_dir()?
    };

    // Resolve bind address: CLI > config
    let bind_str = if cli.bind != "0.0.0.0:8082" { &cli.bind } else { &cfg.bind };
    let addr: SocketAddr = bind_str.parse()?;

    let domain_roots: std::collections::HashMap<String, std::path::PathBuf> = cfg.domain_roots
        .iter()
        .map(|(k, v)| (k.clone(), std::path::PathBuf::from(v)))
        .collect();
    let web_server = Arc::new(WebServer::new(
        root_dir,
        domain_roots,
        cfg.index_files.clone(),
        cfg.php_mode.clone(),
        cfg.php_binary.clone(),
    )?);

    let listener = TcpListener::bind(addr).await?;
    info!("ruph listening on {}", addr);

    let tls_config = if cli.tls || cfg.tls {
        Some(Arc::new(ssl::build_tls_config(&ssl_dir)?))
    } else {
        None
    };

    loop {
        let (stream, remote_addr) = listener.accept().await?;
        let web_server = web_server.clone();
        let tls_config = tls_config.clone();

        tokio::task::spawn(async move {
            if let Some(config) = tls_config {
                // Use LazyConfigAcceptor to peek at SNI before completing handshake
                let acceptor = tokio_rustls::LazyConfigAcceptor::new(
                    rustls::server::Acceptor::default(),
                    stream,
                );
                let start = match acceptor.await {
                    Ok(start) => start,
                    Err(err) => {
                        error!("TLS accept error from {} (no ClientHello): {}", remote_addr, err);
                        return;
                    }
                };

                let sni = start.client_hello().server_name()
                    .unwrap_or("<no SNI>")
                    .to_string();

                match start.into_stream(config).await {
                    Ok(tls_stream) => {
                        info!("TLS established from {} [{}]", remote_addr, sni);
                        let io = TokioIo::new(tls_stream);
                        let sni_clone = sni.clone();
                        let service = service_fn(move |req| {
                            let sni = sni_clone.clone();
                            let ws = web_server.clone();
                            async move { handle_request(req, ws, remote_addr, Some(sni)).await }
                        });
                        if let Err(err) = http1::Builder::new().serve_connection(io, service).await {
                            error!("Error serving TLS connection from {} [{}]: {}", remote_addr, sni, err);
                        }
                    }
                    Err(err) => {
                        error!("TLS handshake error from {} [{}]: {}", remote_addr, sni, err);
                    }
                }
            } else {
                let io = TokioIo::new(stream);
                let service = service_fn(move |req| {
                    let ws = web_server.clone();
                    async move { handle_request(req, ws, remote_addr, None).await }
                });
                if let Err(err) = http1::Builder::new().serve_connection(io, service).await {
                    error!("Error serving connection from {}: {}", remote_addr, err);
                }
            }
        });
    }
}

async fn handle_request(
    req: Request<IncomingBody>,
    web_server: Arc<WebServer>,
    remote_addr: SocketAddr,
    sni: Option<String>,
) -> Result<Response<ResponseBody>, Infallible> {
    let host = req.headers().get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-");
    let domain = sni.as_deref().unwrap_or(host);
    info!("{} {} {} [{}]", remote_addr, req.method(), req.uri(), domain);

    let response = match web_server.handle_request(req).await {
        Ok(resp) => resp,
        Err(e) => {
            error!("Request error: {}", e);
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header("content-type", "text/plain")
                .body(Full::new(Bytes::from("Internal Server Error")))
                .unwrap()
        }
    };

    Ok(response)
}

fn init_logging(level: &str) -> Result<()> {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};

    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(level))?;

    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(filter)
        .init();

    Ok(())
}
