use anyhow::{Result, anyhow};
use clap::Parser;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{error, info};
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
mod web_server;
mod ssl;
mod acme;

use crate::web_server::WebServer;
use crate::{ssl, acme};
use crate::{ssl, acme};

type ResponseBody = Full<Bytes>;

#[derive(Parser, Debug)]
#[command(name = "ruph")]
#[command(about = "Rust + PHP-ish web server", long_about = None)]
struct Cli {
    /// Bind address, e.g. 0.0.0.0:8082
    #[arg(long, default_value = "0.0.0.0:8082")]
    bind: String,

    /// Root directory to serve
    #[arg(long, default_value = "")]
    root: String,

    /// Generate a new TLS certificate: email@domain.com,example.com
    #[arg(long)]
    new_cert: Option<String>,

    /// Enable TLS (uses certs from ~/.ruph/ssl)
    #[arg(long, default_value_t = false)]
    tls: bool,

    /// Log level (error, warn, info, debug, trace)
    #[arg(long, default_value = "info")]
    log_level: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(&cli.log_level)?;

    let ssl_dir = ssl::default_ssl_dir();
    ssl::warn_expiring(&ssl_dir, 30);

    if let Some(spec) = &cli.new_cert {
        let parts: Vec<&str> = spec.split(",").collect();
        if parts.len() != 2 {
            return Err(anyhow!("--new-cert must be email@domain,example.com"));
        }
        let email = parts[0].trim();
        let domain = parts[1].trim();
        acme::issue_cert(email, domain, &ssl_dir).await?;
    }


    let addr: SocketAddr = cli.bind.parse()?;
    let root_dir = if cli.root.trim().is_empty() {
        std::env::current_dir()?
    } else {
        cli.root.into()
    };

    let web_server = Arc::new(WebServer::new(root_dir)?);

    let listener = TcpListener::bind(addr).await?;
    info!("ruph listening on {}", addr);

    let tls_acceptor = if cli.tls {
        let config = ssl::build_tls_config(&ssl_dir)?;
        Some(tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(config)))
    } else {
        None
    };

    loop {
        let (stream, remote_addr) = listener.accept().await?;
        let web_server = web_server.clone();
        let tls_acceptor = tls_acceptor.clone();

        tokio::task::spawn(async move {
            if let Some(acceptor) = tls_acceptor {
                match acceptor.accept(stream).await {
                    Ok(tls_stream) => {
                        let io = TokioIo::new(tls_stream);
                        let service = service_fn(move |req| handle_request(req, web_server.clone(), remote_addr));
                        if let Err(err) = http1::Builder::new().serve_connection(io, service).await {
                            error!("Error serving TLS connection from {}: {}", remote_addr, err);
                        }
                    }
                    Err(err) => {
                        error!("TLS accept error from {}: {}", remote_addr, err);
                    }
                }
            } else {
                let io = TokioIo::new(stream);
                let service = service_fn(move |req| handle_request(req, web_server.clone(), remote_addr));
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
    _remote_addr: SocketAddr,
) -> Result<Response<ResponseBody>, Infallible> {
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
