use anyhow::{Result, anyhow};
use clap::Parser;
use hyper::header::{HeaderValue, HOST};
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{error, info, warn};
use hyper::service::service_fn;
use hyper::{body::Incoming as IncomingBody, Request, Response, StatusCode};
use std::convert::Infallible;
use tokio::net::TcpListener;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as AutoBuilder;

mod ast_php_processor;
mod embedded_php_processor;
mod php_processor;
mod web_server;
mod config;
mod ssl;
mod acme;
mod status;

use crate::web_server::{WebServer, RuphBody};

type ResponseBody = RuphBody;

/// Writes per-domain log lines to configured log files (plain text, no ANSI).
struct DomainLogger {
    default: Option<std::sync::Mutex<std::io::BufWriter<std::fs::File>>>,
    files: std::collections::HashMap<String, std::sync::Mutex<std::io::BufWriter<std::fs::File>>>,
    prefix_files: Vec<(String, std::sync::Mutex<std::io::BufWriter<std::fs::File>>)>,
}

impl DomainLogger {
    fn new(
        default_log: &Option<String>,
        domain_logs: &std::collections::HashMap<String, String>,
        prefix_logs: &[(String, String)],
    ) -> Result<Self> {
        let default = if let Some(path) = default_log {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| anyhow!("Cannot open default log file '{}': {}", path, e))?;
            eprintln!("  log: * -> {}", path);
            Some(std::sync::Mutex::new(std::io::BufWriter::new(file)))
        } else {
            None
        };
        let mut files = std::collections::HashMap::new();
        for (domain, path) in domain_logs {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| anyhow!("Cannot open log file '{}' for domain '{}': {}", path, domain, e))?;
            eprintln!("  log: {} -> {}", domain, path);
            files.insert(domain.clone(), std::sync::Mutex::new(std::io::BufWriter::new(file)));
        }
        let mut prefix_files_vec = Vec::new();
        for (prefix, path) in prefix_logs {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| anyhow!("Cannot open log file '{}' for prefix '{}': {}", path, prefix, e))?;
            eprintln!("  log: {}* -> {}", prefix, path);
            prefix_files_vec.push((prefix.clone(), std::sync::Mutex::new(std::io::BufWriter::new(file))));
        }
        Ok(DomainLogger { default, files, prefix_files: prefix_files_vec })
    }

    fn log(&self, domain: &str, line: &str) {
        use std::io::Write;
        let bare = domain.split(':').next().unwrap_or(domain);
        // 1. Exact match
        if let Some(mutex) = self.files.get(bare) {
            if let Ok(mut w) = mutex.lock() {
                let _ = writeln!(w, "{}", line);
                let _ = w.flush();
            }
            return;
        }
        // 2. Prefix match (longest wins)
        let mut best: Option<&std::sync::Mutex<std::io::BufWriter<std::fs::File>>> = None;
        let mut best_len = 0;
        for (prefix, mutex) in &self.prefix_files {
            if prefix.len() > best_len
                && bare.starts_with(prefix.as_str())
                && (bare.len() == prefix.len()
                    || bare.as_bytes().get(prefix.len()) == Some(&b'.'))
            {
                best = Some(mutex);
                best_len = prefix.len();
            }
        }
        if let Some(mutex) = best {
            if let Ok(mut w) = mutex.lock() {
                let _ = writeln!(w, "{}", line);
                let _ = w.flush();
            }
            return;
        }
        // 3. Default
        if let Some(mutex) = &self.default {
            if let Ok(mut w) = mutex.lock() {
                let _ = writeln!(w, "{}", line);
                let _ = w.flush();
            }
        }
    }

    fn log_request(&self, domain: &str, remote_addr: &SocketAddr, method: &hyper::Method, uri: &hyper::Uri, status: u16, is_tls: bool, redirect_to: Option<&str>) {
        let now = chrono::Local::now();
        let level = if status >= 500 { "ERROR" } else if status >= 400 { " WARN" } else { " INFO" };
        let path = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or(uri.path());
        let proto = if is_tls { "S" } else { "-" };
        let redir = redirect_to.map(|r| format!(" -> {}", r)).unwrap_or_default();
        self.log(domain, &format!("{} {} [{}] {} [{}] {} {} {}{}", now.format("%H:%M:%S"), level, status, proto, domain, remote_addr, method, path, redir));
    }

    fn log_error(&self, domain: &str, remote_addr: &SocketAddr, method: &hyper::Method, uri: &hyper::Uri, err: &str) {
        let now = chrono::Local::now();
        let path = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or(uri.path());
        self.log(domain, &format!("{} ERROR [{}] {} {} {} - {}", now.format("%H:%M:%S"), domain, remote_addr, method, path, err));
    }
}

#[derive(Parser, Debug)]
#[command(name = "ruph")]
#[command(about = "Rust + PHP-ish web server", long_about = None)]
struct Cli {
    /// Root directory to serve
    #[arg(value_name = "DOCROOT")]
    root: Option<String>,

    /// HTTPS bind address, e.g. 0.0.0.0:8082
    #[arg(long = "bind-https", alias = "bind", value_name = "ADDR")]
    bind_https: Option<String>,

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

    /// PHP binary to use (e.g. php-cgi, /usr/local/bin/php-cgi). Overrides config file.
    #[arg(long, value_name = "BINARY")]
    php_binary: Option<String>,

    /// Log level (error, warn, info, debug, trace)
    #[arg(long, default_value = "info")]
    log_level: String,

    /// Optional plain-HTTP bind address, e.g. 0.0.0.0:80
    #[arg(long = "bind-http", alias = "http-bind", value_name = "ADDR")]
    bind_http: Option<String>,

    /// Enable logging to stdout/console (default: off)
    #[arg(long, default_value_t = false)]
    log_console: bool,
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
        eprintln!("  config: (none — using defaults)");
        config::Config::default()
    };

    // CLI overrides config (CLI takes precedence)
    let log_level = if cli.log_level != "info" { &cli.log_level } else { &cfg.log_level };
    let log_console = cli.log_console || cfg.log_console;
    if log_console {
        init_logging(log_level)?;
    }

    let domain_logger = Arc::new(DomainLogger::new(&cfg.default_log, &cfg.domain_logs, &cfg.prefix_logs)?);

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
        return Ok(());
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

    // Resolve HTTPS bind address: CLI > config
    let bind_str = cli.bind_https.as_deref().unwrap_or(&cfg.bind);
    let addr: SocketAddr = bind_str.parse()?;

    let domain_roots: std::collections::HashMap<String, std::path::PathBuf> = cfg.domain_roots
        .iter()
        .map(|(k, v)| (k.clone(), std::path::PathBuf::from(v)))
        .collect();
    let prefix_roots: Vec<(String, std::path::PathBuf)> = cfg.prefix_roots
        .iter()
        .map(|(k, v)| (k.clone(), std::path::PathBuf::from(v)))
        .collect();
    // CLI --php-binary overrides config file php.binary
    let php_binary = cli.php_binary.clone().or(cfg.php_binary.clone());

    let php_error_log: Option<std::sync::Arc<dyn Fn(&str, &str) + Send + Sync>> = {
        let dl = domain_logger.clone();
        Some(std::sync::Arc::new(move |domain: &str, line: &str| {
            let now = chrono::Local::now();
            dl.log(domain, &format!("{} PHP: {}", now.format("%H:%M:%S"), line));
        }))
    };

    let server_stats = Arc::new(status::ServerStats::new(cfg.rate_window));

    let web_server = Arc::new(WebServer::new(
        root_dir,
        domain_roots,
        prefix_roots,
        cfg.index_files.clone(),
        cfg.php_mode.clone(),
        php_binary.clone(),
        php_error_log.clone(),
        Some(server_stats.clone()),
    )?);

    let status_page_path: Option<String> = cfg.status_page.clone();
    if let Some(ref sp) = status_page_path {
        eprintln!("  status page: {}", sp);
    }

    let listener = TcpListener::bind(addr).await?;
    eprintln!("ruph listening on {}", addr);

    let tls_config = if cli.tls || cfg.tls {
        Some(Arc::new(ssl::build_tls_config(&ssl_dir)?))
    } else {
        None
    };

    // Optional plain-HTTP listener (CLI overrides config)
    let http_bind_str = cli.bind_http.as_deref().or(cfg.http_bind.as_deref());
    let http_listener = if let Some(hb) = http_bind_str {
        let http_addr: SocketAddr = hb.parse()?;
        let hl = TcpListener::bind(http_addr).await?;
        eprintln!("ruph HTTP listening on {}", http_addr);
        Some(hl)
    } else {
        None
    };

    // Separate WebServer for plain-HTTP if http_docroot is configured
    let http_web_server = if http_listener.is_some() {
        let http_root = if let Some(ref d) = cfg.http_docroot {
            std::path::PathBuf::from(d)
        } else if let Some(ref d) = cfg.docroot {
            // Fall back to main docroot if http_docroot not specified
            std::path::PathBuf::from(d)
        } else {
            // Fall back to CLI docroot
            web_server.root_dir.clone()
        };
        eprintln!("  http docroot: {}", http_root.display());
        Some(Arc::new(WebServer::new(
            http_root,
            std::collections::HashMap::new(), // No per-domain roots for HTTP
            Vec::new(),                        // No prefix roots for HTTP
            cfg.index_files.clone(),
            cfg.php_mode.clone(),
            php_binary,
            php_error_log,
            Some(server_stats.clone()),
        )?))
    } else {
        None
    };

    loop {
        // When both listeners are active, accept from whichever is ready first
        if let Some(ref hl) = http_listener {
            tokio::select! {
                result = listener.accept() => {
                    let (stream, remote_addr) = result?;
                    let web_server = web_server.clone();
                    let tls_config = tls_config.clone();
                    let dl = domain_logger.clone();
                    let stats = server_stats.clone();
                    let sp = status_page_path.clone();
                    tokio::task::spawn(async move {
                        serve_connection(stream, remote_addr, web_server, tls_config, dl, stats, sp).await;
                    });
                }
                result = hl.accept() => {
                    let (stream, remote_addr) = result?;
                    let ws = http_web_server.as_ref().unwrap().clone();
                    let dl = domain_logger.clone();
                    let stats = server_stats.clone();
                    let sp = status_page_path.clone();
                    tokio::task::spawn(async move {
                        serve_connection(stream, remote_addr, ws, None, dl, stats, sp).await;
                    });
                }
            }
        } else {
            let (stream, remote_addr) = listener.accept().await?;
            let web_server = web_server.clone();
            let tls_config = tls_config.clone();
            let dl = domain_logger.clone();
            let stats = server_stats.clone();
            let sp = status_page_path.clone();
            tokio::task::spawn(async move {
                serve_connection(stream, remote_addr, web_server, tls_config, dl, stats, sp).await;
            });
        }
    }
}

/// Shared hyper connection builder with tuned H2 window sizes.
/// 1 MiB initial windows mean the first response fits in one window
/// without waiting for flow-control ACKs, reducing TTFB on larger pages.
fn http_builder() -> AutoBuilder<TokioExecutor> {
    let mut b = AutoBuilder::new(TokioExecutor::new());
    b.http2()
        .initial_stream_window_size(1024 * 1024)
        .initial_connection_window_size(2 * 1024 * 1024);
    b
}

/// Serve a single TCP connection, optionally upgrading to TLS.
async fn serve_connection(
    stream: tokio::net::TcpStream,
    remote_addr: SocketAddr,
    web_server: Arc<WebServer>,
    tls_config: Option<Arc<rustls::ServerConfig>>,
    domain_logger: Arc<DomainLogger>,
    stats: Arc<status::ServerStats>,
    status_page_path: Option<String>,
) {
    stats.connection_opened();
    let conn_stats = stats.clone();

    // Disable Nagle's algorithm so TLS handshake packets are sent immediately
    // rather than being buffered waiting for more data. Material impact on
    // handshake latency (can save 40ms+ on some networks).
    if let Err(e) = stream.set_nodelay(true) {
        warn!("Failed to set TCP_NODELAY: {}", e);
    }

    if let Some(config) = tls_config {
        let acceptor = tokio_rustls::TlsAcceptor::from(config);
        match acceptor.accept(stream).await {
            Ok(tls_stream) => {
                let sni = tls_stream
                    .get_ref()
                    .1
                    .server_name()
                    .unwrap_or("<no SNI>")
                    .to_string();
                info!("[{}] TLS from {}", sni, remote_addr);
                let io = TokioIo::new(tls_stream);
                let sni_clone = sni.clone();
                let service = service_fn(move |req| {
                    let sni = sni_clone.clone();
                    let ws = web_server.clone();
                    let dl = domain_logger.clone();
                    let st = stats.clone();
                    let sp = status_page_path.clone();
                    async move { handle_request(req, ws, remote_addr, Some(sni), dl, st, sp).await }
                });
                let builder = http_builder();
                if let Err(err) = builder.serve_connection(io, service).await {
                    error!("[{}] TLS error from {}: {}", sni, remote_addr, err);
                }
            }
            Err(err) => {
                error!("TLS handshake failed from {}: {}", remote_addr, err);
            }
        }
    } else {
        let io = TokioIo::new(stream);
        let service = service_fn(move |req| {
            let ws = web_server.clone();
            let dl = domain_logger.clone();
            let st = stats.clone();
            let sp = status_page_path.clone();
            async move { handle_request(req, ws, remote_addr, None, dl, st, sp).await }
        });
        let builder = http_builder();
        if let Err(err) = builder.serve_connection(io, service).await {
            error!("Error serving connection from {}: {}", remote_addr, err);
        }
    }

    conn_stats.connection_closed();
}

async fn handle_request(
    mut req: Request<IncomingBody>,
    web_server: Arc<WebServer>,
    remote_addr: SocketAddr,
    sni: Option<String>,
    domain_logger: Arc<DomainLogger>,
    stats: Arc<status::ServerStats>,
    status_page_path: Option<String>,
) -> Result<Response<ResponseBody>, Infallible> {
    // For TLS requests, make SNI authoritative for vhost routing.
    // This avoids host/:authority edge cases across HTTP versions.
    if let Some(ref sni_name) = sni {
        if let Ok(hv) = HeaderValue::from_str(sni_name) {
            req.headers_mut().insert(HOST, hv);
        }
    }

    let host = req.headers().get("host")
        .and_then(|v| v.to_str().ok())
        .or_else(|| req.uri().authority().map(|a| a.as_str()))
        .unwrap_or("-")
        .to_string();
    let is_tls = sni.is_some();
    let domain = sni.unwrap_or_else(|| host.clone());
    let method = req.method().clone();
    let uri = req.uri().clone();

    stats.record_request(remote_addr.ip());

    // Status page intercept — before virtual host routing
    if let Some(ref sp) = status_page_path {
        if uri.path() == sp.as_str() {
            let html = status::render_status_page(&stats);
            let response = Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/html; charset=utf-8")
                .header("cache-control", "no-cache, no-store")
                .body(RuphBody::full(html))
                .unwrap();
            let status_code = response.status().as_u16();
            let path = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or(uri.path());
            let proto = if is_tls { "S" } else { "-" };
            info!(http = status_code, "{} [{}] {} {} {}", proto, domain, remote_addr, method, path);
            domain_logger.log_request(&domain, &remote_addr, &method, &uri, status_code, is_tls, None);
            return Ok(response);
        }
    }

    let response = match web_server.handle_request(req, Some(remote_addr)).await {
        Ok(resp) => resp,
        Err(e) => {
            error!("[{}] {} {} {} - {}", domain, remote_addr, method, uri, e);
            domain_logger.log_error(&domain, &remote_addr, &method, &uri, &e.to_string());
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header("content-type", "text/plain")
                .body(RuphBody::full("Internal Server Error"))
                .unwrap()
        }
    };

    let status = response.status().as_u16();
    let path = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or(uri.path());
    let proto = if is_tls { "S" } else { "-" };
    let location = if (300..400).contains(&status) {
        response.headers().get("location")
            .and_then(|v| v.to_str().ok())
            .map(|v| format!(" -> {}", v))
            .unwrap_or_default()
    } else {
        String::new()
    };
    info!(http = status, "{} [{}] {} {} {}{}", proto, domain, remote_addr, method, path, location);
    domain_logger.log_request(&domain, &remote_addr, &method, &uri, status, is_tls,
        if location.is_empty() { None } else { Some(&location[4..]) });

    Ok(response)
}

fn init_logging(level: &str) -> Result<()> {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};

    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(level))?;

    tracing_subscriber::registry()
        .with(fmt::layer().event_format(RuphFormatter))
        .with(filter)
        .init();

    Ok(())
}

/// Custom log formatter: `HH:MM:SS  INFO [200] [domain] ip:port METHOD /path`
struct RuphFormatter;

impl<S, N> tracing_subscriber::fmt::FormatEvent<S, N> for RuphFormatter
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &tracing_subscriber::fmt::FmtContext<'_, S, N>,
        mut writer: tracing_subscriber::fmt::format::Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        use tracing::Level;

        let now = chrono::Local::now();
        write!(writer, "{} ", now.format("%H:%M:%S"))?;

        let level = *event.metadata().level();
        let (lc, label) = match level {
            Level::ERROR => ("\x1b[31m", "ERROR"),
            Level::WARN  => ("\x1b[33m", " WARN"),
            Level::INFO  => ("\x1b[32m", " INFO"),
            Level::DEBUG => ("\x1b[36m", "DEBUG"),
            Level::TRACE => ("\x1b[35m", "TRACE"),
        };
        write!(writer, "{}{}\x1b[0m ", lc, label)?;

        // Extract fields ourselves so ANSI codes are written directly
        let mut visitor = RuphVisitor::default();
        event.record(&mut visitor);

        if let Some(code) = visitor.http_status {
            let sc = match code {
                200..=299 => "\x1b[32m",
                300..=399 => "\x1b[36m",
                400..=499 => "\x1b[33m",
                500..=599 => "\x1b[31m",
                _ => "",
            };
            write!(writer, "{}[{}]\x1b[0m ", sc, code)?;
        }

        writeln!(writer, "{}", visitor.message)
    }
}

/// Visitor that extracts the message text and optional `http` status field.
#[derive(Default)]
struct RuphVisitor {
    message: String,
    http_status: Option<u16>,
}

impl tracing::field::Visit for RuphVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            use std::fmt::Write;
            let _ = write!(&mut self.message, "{:?}", value);
        }
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        if field.name() == "http" {
            self.http_status = Some(value as u16);
        }
    }
}
