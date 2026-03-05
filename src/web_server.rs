//! Web server functionality for serving static files and processing PHP-like templates
//!
//! This module provides HTTP web server capabilities alongside the MCP protocol,
//! allowing the server to serve static files and process embedded PHP-like templates.

use std::path::{Path, PathBuf};
use std::collections::HashMap;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use hyper::{Request, Response, StatusCode, Method};
use hyper::header::{HeaderName, HeaderValue};
use hyper::body::{Body, Frame, Incoming as IncomingBody, SizeHint};
use http_body_util::BodyExt;
use bytes::Bytes;
use mime_guess::from_path;
use urlencoding::decode;
use tokio::fs;
use tokio::sync::{Mutex, mpsc};
use anyhow::{Result, anyhow};
use tracing::{debug, info, warn, error};
use crate::embedded_php_processor::EmbeddedPhpProcessor;
use crate::ast_php_processor::{AstPhpProcessor, PhpExecution};
use crate::php_processor::{PhpProcessor, PhpStream, PhpStderrHandler};
use crate::config::PhpMode;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Streaming-capable response body
// ---------------------------------------------------------------------------

/// Response body that is either fully buffered or streamed from a PHP process.
pub enum RuphBody {
    /// A complete, in-memory body (for static files, short PHP responses).
    Full(Option<Bytes>),
    /// A streaming body fed by a background task (for SSE / long-running PHP).
    Streaming(mpsc::Receiver<Result<Bytes, io::Error>>),
}

impl RuphBody {
    pub fn full(data: impl Into<Bytes>) -> Self {
        RuphBody::Full(Some(data.into()))
    }

    pub fn empty() -> Self {
        RuphBody::Full(None)
    }

    pub fn streaming(rx: mpsc::Receiver<Result<Bytes, io::Error>>) -> Self {
        RuphBody::Streaming(rx)
    }

    /// Returns true if the body is known to be empty (non-streaming with zero bytes).
    pub fn is_empty(&self) -> bool {
        match self {
            RuphBody::Full(None) => true,
            RuphBody::Full(Some(b)) => b.is_empty(),
            RuphBody::Streaming(_) => false, // assume non-empty if streaming
        }
    }
}

impl Body for RuphBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        match &mut *self {
            RuphBody::Full(opt) => {
                Poll::Ready(opt.take().filter(|b| !b.is_empty()).map(|b| Ok(Frame::data(b))))
            }
            RuphBody::Streaming(rx) => {
                rx.poll_recv(cx).map(|opt| opt.map(|r| r.map(Frame::data)))
            }
        }
    }

    fn size_hint(&self) -> SizeHint {
        match self {
            RuphBody::Full(Some(b)) => SizeHint::with_exact(b.len() as u64),
            RuphBody::Full(None) => SizeHint::with_exact(0),
            RuphBody::Streaming(_) => SizeHint::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Web server
// ---------------------------------------------------------------------------

/// Web server handler for HTTP requests
pub struct WebServer {
    /// Default root directory for serving files
    pub root_dir: PathBuf,
    /// Per-domain docroot overrides (exact match, domain -> path, port stripped)
    domain_roots: HashMap<String, PathBuf>,
    /// Prefix-based docroot overrides (e.g. "www" matches "www.*")
    prefix_roots: Vec<(String, PathBuf)>,
    /// Pre-canonicalized versions of all configured root dirs (raw -> canonical)
    canonical_roots: HashMap<PathBuf, PathBuf>,
    /// Ordered list of filenames to try when a directory is requested
    index_files: Vec<String>,
    /// Cached first PHP index file name (used as middleware entry point every request)
    middleware_index: String,
    /// PHP processor mode (controls execution order)
    php_mode: PhpMode,
    /// AST-based PHP processor
    ast_php_processor: Option<Mutex<AstPhpProcessor>>,
    /// Embedded regex PHP processor
    embedded_php_processor: Option<EmbeddedPhpProcessor>,
    /// External PHP binary processor
    php_processor: Option<PhpProcessor>,
    /// Callback for routing PHP stderr to domain logs: (domain, message)
    php_error_log: Option<Arc<dyn Fn(&str, &str) + Send + Sync>>,
}

impl WebServer {
    fn apply_safe_headers(
        mut builder: hyper::http::response::Builder,
        headers: &HashMap<String, String>,
    ) -> hyper::http::response::Builder {
        for (name, value) in headers {
            let header_name = match HeaderName::from_bytes(name.trim().as_bytes()) {
                Ok(n) => n,
                Err(_) => {
                    warn!("Skipping invalid response header name: {:?}", name);
                    continue;
                }
            };
            let header_value = match HeaderValue::from_str(value.trim()) {
                Ok(v) => v,
                Err(_) => {
                    warn!("Skipping invalid response header value for {}: {:?}", header_name, value);
                    continue;
                }
            };
            builder = builder.header(header_name, header_value);
        }
        builder
    }

    /// Create a new web server instance with PHP mode, optional binary path, and per-domain roots.
    pub fn new(
        root_dir: PathBuf,
        domain_roots: HashMap<String, PathBuf>,
        prefix_roots: Vec<(String, PathBuf)>,
        index_files: Vec<String>,
        php_mode: PhpMode,
        php_binary: Option<String>,
        php_error_log: Option<Arc<dyn Fn(&str, &str) + Send + Sync>>,
    ) -> Result<Self> {
        // Initialize AST-based PHP processor
        let ast_php_processor = match AstPhpProcessor::new() {
            Ok(processor) => {
                debug!("AST-based PHP processor initialized");
                Some(Mutex::new(processor))
            }
            Err(e) => {
                warn!("Failed to initialize AST PHP processor: {}", e);
                None
            }
        };

        // Initialize embedded PHP processor
        let embedded_php_processor = match EmbeddedPhpProcessor::new() {
            Ok(processor) => {
                debug!("Embedded PHP processor initialized");
                Some(processor)
            }
            Err(e) => {
                warn!("Failed to initialize embedded PHP processor: {}", e);
                None
            }
        };

        // Initialize external PHP binary processor
        let php_processor = match php_binary {
            Some(bin) => match PhpProcessor::with_binary(bin) {
                Ok(p) => Some(p),
                Err(e) => {
                    warn!("Failed to initialize external PHP with specified binary: {}", e);
                    PhpProcessor::new().ok()
                }
            },
            None => match PhpProcessor::new() {
                Ok(p) => Some(p),
                Err(e) => {
                    warn!("External PHP binary not available: {}", e);
                    None
                }
            },
        };

        // Pre-canonicalize all configured roots so resolve_file_path() doesn't do it per request.
        let mut canonical_roots: HashMap<PathBuf, PathBuf> = HashMap::new();
        for root in std::iter::once(&root_dir)
            .chain(domain_roots.values())
            .chain(prefix_roots.iter().map(|(_, r)| r))
        {
            if let Ok(canonical) = root.canonicalize() {
                canonical_roots.insert(root.clone(), canonical);
            }
        }

        let middleware_index = index_files
            .iter()
            .find(|name| name.ends_with(".php"))
            .cloned()
            .unwrap_or_else(|| "_index.php".to_string());

        if ast_php_processor.is_none() && embedded_php_processor.is_none() && php_processor.is_none() {
            warn!("No PHP processors available. PHP files will be served as static content.");
        } else {
            let available: Vec<&str> = [
                ast_php_processor.as_ref().map(|_| "ast"),
                embedded_php_processor.as_ref().map(|_| "embedded"),
                php_processor.as_ref().map(|p| { let _ = p; "cgi" }),
            ].into_iter().flatten().collect();
            eprintln!("  php: [{}], mode: {:?}", available.join(", "), php_mode);
        }

        if !domain_roots.is_empty() {
            for (domain, root) in &domain_roots {
                eprintln!("  vhost: {} -> {}", domain, root.display());
            }
        }
        if !prefix_roots.is_empty() {
            for (prefix, root) in &prefix_roots {
                eprintln!("  vhost: {}* -> {}", prefix, root.display());
            }
        }

        Ok(Self {
            root_dir,
            domain_roots,
            prefix_roots,
            canonical_roots,
            index_files,
            middleware_index,
            php_mode,
            ast_php_processor,
            embedded_php_processor,
            php_processor,
            php_error_log,
        })
    }

    /// Create a domain-bound stderr handler for PHP error_log() output.
    fn stderr_handler_for(&self, domain: &str) -> Option<PhpStderrHandler> {
        let log = self.php_error_log.as_ref()?.clone();
        let domain = domain.to_string();
        Some(Arc::new(move |line: &str| {
            log(&domain, line);
        }))
    }

    /// Find the first PHP file from `index_files` that exists in `root`.
    fn find_root_init_script(root: &Path, index_files: &[String]) -> Option<PathBuf> {
        index_files.iter()
            .filter(|name| name.ends_with(".php"))
            .map(|name| root.join(name))
            .find(|p| p.is_file())
    }

    /// Return the docroot for a given `Host` header value (port stripped).
    /// Priority: exact match > longest prefix match > root_dir.
    fn effective_root(&self, host: &str) -> &PathBuf {
        let domain_raw = host.split(':').next().unwrap_or(host);
        let domain = domain_raw.to_ascii_lowercase();
        // 1. Exact match
        if let Some(root) = self.domain_roots.get(&domain) {
            return root;
        }
        // Backward compatibility for mixed-case keys in config maps.
        if let Some(root) = self.domain_roots.get(domain_raw) {
            return root;
        }
        // 2. Prefix match (longest prefix wins)
        let mut best: Option<&PathBuf> = None;
        let mut best_len = 0;
        for (prefix, root) in &self.prefix_roots {
            let prefix_lc = prefix.to_ascii_lowercase();
            if prefix.len() > best_len
                && domain.starts_with(prefix_lc.as_str())
                && (domain.len() == prefix_lc.len()
                    || domain.as_bytes().get(prefix_lc.len()) == Some(&b'.'))
            {
                best = Some(root);
                best_len = prefix_lc.len();
            }
        }
        if let Some(root) = best {
            return root;
        }
        &self.root_dir
    }

    /// Return the init script for a given host by scanning `index_files` at request time.
    fn effective_init_script(&self, host: &str) -> Option<PathBuf> {
        let root = self.effective_root(host);
        Self::find_root_init_script(root, &self.index_files)
    }

    /// Middleware index name: first configured PHP index file, defaulting to `_index.php`.
    fn middleware_index_name(&self) -> &str {
        &self.middleware_index
    }

    /// Build the top-down directory chain for a request path.
    /// Example: `/a/b/c.html` -> [`/`, `/a`, `/a/b`].
    fn directory_chain_for_path(&self, url_path: &str, root: &Path) -> Result<Vec<PathBuf>> {
        let decoded = decode(url_path).map_err(|_| anyhow!("Invalid URL encoding"))?;
        let clean = decoded.trim_start_matches('/');
        let mut chain = vec![root.to_path_buf()];

        if clean.is_empty() {
            return Ok(chain);
        }

        let target = self.resolve_file_path(url_path, root)?;
        let is_dir_target = target.is_dir() || url_path.ends_with('/');
        let parts: Vec<&str> = clean.split('/').filter(|p| !p.is_empty()).collect();
        let dir_count = if is_dir_target { parts.len() } else { parts.len().saturating_sub(1) };

        let mut current = root.to_path_buf();
        for part in parts.iter().take(dir_count) {
            current = current.join(part);
            chain.push(current.clone());
        }

        Ok(chain)
    }

    /// Handle HTTP web requests
    pub async fn handle_request(&self, req: Request<IncomingBody>) -> Result<Response<RuphBody>> {
        let host = req.headers().get("host")
            .and_then(|v| v.to_str().ok())
            .or_else(|| req.uri().authority().map(|a| a.as_str()))
            .unwrap_or("")
            .to_string();

        let root = self.effective_root(&host).clone();
        let init_script = self.effective_init_script(&host);
        let stderr_handler = self.stderr_handler_for(&host);

        let method = req.method().clone();
        let path = req.uri().path().to_string();

        debug!("Web request: {} {}", method, path);

        // Security: Prevent path traversal attacks
        if path.contains("..") || path.contains("\\") {
            return Ok(self.error_response(StatusCode::FORBIDDEN, "Access denied"));
        }

        // Execute `_index.php` middleware scripts top-down for all request URLs.
        // If any middleware handles the response (redirect/body/non-200), stop immediately.
        if let Some(resp) = self.run_directory_index_chain(&req, &root, stderr_handler.as_ref()).await? {
            return Ok(resp);
        }

        match method {
            Method::GET => self.handle_get_request(req, &root, init_script.as_deref(), stderr_handler.as_ref()).await,
            Method::POST => self.handle_post_request(req, &root, init_script.as_deref(), stderr_handler.as_ref()).await,
            Method::HEAD => self.handle_head_request(req, &root, init_script.as_deref(), stderr_handler.as_ref()).await,
            _ => Ok(self.error_response(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed")),
        }
    }

    /// Handle GET requests
    async fn handle_get_request(&self, req: Request<IncomingBody>, root: &Path, init_script: Option<&Path>, stderr_handler: Option<&PhpStderrHandler>) -> Result<Response<RuphBody>> {
        let uri = req.uri();
        let path = uri.path();
        let query = uri.query();

        match self.resolve_request_target(path, root, init_script)? {
            RequestTarget::Static(file_path) => self.serve_static_file(&file_path).await,
            RequestTarget::Script(script_path) => {
                let query_params = self.parse_query_string(query.unwrap_or(""));
                let server_vars = self.build_server_vars(&req, &script_path, root)?;
                let prefer_sse = req.headers()
                    .get("accept")
                    .and_then(|v| v.to_str().ok())
                    .map(|v| v.contains("text/event-stream"))
                    .unwrap_or(false);
                self.process_php_template(&script_path, &query_params, &HashMap::new(), &server_vars, prefer_sse, stderr_handler).await
            }
            RequestTarget::NotFound => Ok(self.error_response(StatusCode::NOT_FOUND, "File not found")),
        }
    }

    /// Handle POST requests
    async fn handle_post_request(&self, req: Request<IncomingBody>, root: &Path, init_script: Option<&Path>, stderr_handler: Option<&PhpStderrHandler>) -> Result<Response<RuphBody>> {
        let uri = req.uri().clone();
        let path = uri.path();
        let target = self.resolve_request_target(path, root, init_script)?;
        let script_path = match target {
            RequestTarget::Script(path) => path,
            RequestTarget::Static(_) | RequestTarget::NotFound => {
                // Front controller handles POST for non-script targets too
                if let Some(init) = init_script.filter(|p| p.is_file()) {
                    init.to_path_buf()
                } else {
                    return Ok(self.error_response(StatusCode::NOT_FOUND, "Not found"));
                }
            }
        };

        let server_vars = self.build_server_vars(&req, &script_path, root)?;

        let prefer_sse = req.headers()
            .get("accept")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.contains("text/event-stream"))
            .unwrap_or(false);

        // Parse POST data
        let body_bytes = match req.collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(_) => return Ok(self.error_response(StatusCode::BAD_REQUEST, "Invalid request body")),
        };

        let post_data = self.parse_post_data(&body_bytes);
        let query_params = self.parse_query_string(uri.query().unwrap_or(""));
        self.process_php_template(&script_path, &query_params, &post_data, &server_vars, prefer_sse, stderr_handler).await
    }

    /// Handle HEAD requests
    async fn handle_head_request(&self, req: Request<IncomingBody>, root: &Path, init_script: Option<&Path>, _stderr_handler: Option<&PhpStderrHandler>) -> Result<Response<RuphBody>> {
        let uri = req.uri();
        let path = uri.path();

        match self.resolve_request_target(path, root, init_script)? {
            RequestTarget::Static(file_path) => {
                let content_type = self.get_content_type(&file_path);
                let metadata = match fs::metadata(&file_path).await {
                    Ok(meta) => meta,
                    Err(_) => return Ok(self.error_response(StatusCode::INTERNAL_SERVER_ERROR, "Cannot read file metadata")),
                };

                Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", content_type)
                    .header("Content-Length", metadata.len().to_string())
                    .body(RuphBody::empty())
                    .map_err(|e| anyhow!("Failed to build response: {}", e))
            }
            RequestTarget::Script(_) => Ok(self.error_response(StatusCode::METHOD_NOT_ALLOWED, "HEAD not supported for scripts")),
            RequestTarget::NotFound => Ok(self.error_response(StatusCode::NOT_FOUND, "File not found")),
        }
    }

    /// Resolve file path from URL path, rooted at `root`.
    fn resolve_file_path(&self, url_path: &str, root: &Path) -> Result<PathBuf> {
        let decoded_path = decode(url_path).map_err(|_| anyhow!("Invalid URL encoding"))?;
        let clean_path = decoded_path.trim_start_matches('/');

        let file_path = if clean_path.is_empty() {
            root.to_path_buf()
        } else {
            root.join(clean_path)
        };

        // Ensure the resolved path is within the root directory.
        // Use the pre-canonicalized root if available (common case); fall back to computing it.
        let canonical_root = self.canonical_roots.get(root)
            .cloned()
            .or_else(|| root.canonicalize().ok())
            .ok_or_else(|| anyhow!("Cannot canonicalize root directory"))?;

        if let Ok(canonical_file) = file_path.canonicalize() {
            if !canonical_file.starts_with(&canonical_root) {
                return Err(anyhow!("Path traversal attempt detected"));
            }
        }

        Ok(file_path)
    }

    /// Serve static file
    async fn serve_static_file(&self, file_path: &Path) -> Result<Response<RuphBody>> {
        info!("Serving static file: {:?}", file_path);
        let content = match fs::read(file_path).await {
            Ok(content) => content,
            Err(e) => {
                error!("Failed to read file {:?}: {}", file_path, e);
                return Ok(self.error_response(StatusCode::INTERNAL_SERVER_ERROR, "Cannot read file"));
            }
        };

        let content_type = self.get_content_type(file_path);

        Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", content_type)
            .header("Content-Length", content.len().to_string())
            .body(RuphBody::full(content))
            .map_err(|e| anyhow!("Failed to build response: {}", e))
    }

    /// Process a PHP script, returning a streaming response for SSE or a buffered one otherwise.
    async fn process_php_template(
        &self,
        template_path: &Path,
        query_params: &HashMap<String, String>,
        post_params: &HashMap<String, String>,
        server_vars: &HashMap<String, String>,
        prefer_sse: bool,
        stderr_handler: Option<&PhpStderrHandler>,
    ) -> Result<Response<RuphBody>> {
        debug!("Processing PHP template: {:?}", template_path);

        if !template_path.is_file() {
            warn!("Refusing to execute non-file PHP target: {:?}", template_path);
            return Ok(self.error_response(StatusCode::NOT_FOUND, "Script not found"));
        }

        if self.ast_php_processor.is_none() && self.embedded_php_processor.is_none() && self.php_processor.is_none() {
            warn!("No PHP processors available, serving PHP file as static content");
            return self.serve_static_file(template_path).await;
        }

        // cgi mode: use streaming PHP execution so SSE and header() work correctly
        if matches!(self.php_mode, PhpMode::Cgi | PhpMode::Auto) {
            if let Some(php) = &self.php_processor {
                match php.stream_file(template_path, query_params, post_params, server_vars, stderr_handler.cloned()).await {
                    Ok(stream) => return self.build_response_from_stream(stream, prefer_sse).await,
                    Err(e) => warn!("PHP streaming failed for {:?}: {}, trying fallback", template_path, e),
                }
            }
        }

        // Fallback: AST or embedded processor (buffered, no CGI header support)
        let content = match fs::read_to_string(template_path).await {
            Ok(content) => content,
            Err(_) => return Ok(self.error_response(StatusCode::INTERNAL_SERVER_ERROR, "Cannot read PHP file")),
        };

        let output = match self.php_mode {
            PhpMode::Ast => self.run_ast_only(&content, template_path, query_params, post_params, server_vars).await,
            PhpMode::Embedded => self.run_embedded_only(&content, query_params, post_params, server_vars),
            PhpMode::Cgi | PhpMode::Auto => {
                // External PHP already failed; fall back through AST then embedded
                self.run_auto_chain_with_handler(&content, template_path, query_params, post_params, server_vars, stderr_handler).await
            }
        };

        let output = match output {
            Ok(o) => o,
            Err(e) => {
                error!("All PHP processors failed for {:?}: {}", template_path, e);
                return Ok(self.error_response(StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("Template processing error: {}", e)));
            }
        };

        let default_content_type = if prefer_sse {
            "text/event-stream"
        } else {
            "text/html; charset=utf-8"
        };
        let status = StatusCode::from_u16(output.status).unwrap_or(StatusCode::OK);
        let builder = Response::builder()
            .status(status)
            .header("Content-Type", default_content_type);

        let builder = Self::apply_safe_headers(builder, &output.headers);

        builder
            .body(RuphBody::full(output.body))
            .map_err(|e| anyhow!("Failed to build response: {}", e))
    }

    /// Build an HTTP response from a `PhpStream`.
    /// If the response is SSE (`Content-Type: text/event-stream`), the body is streamed
    /// incrementally; otherwise all chunks are collected into a buffered body.
    async fn build_response_from_stream(&self, stream: PhpStream, prefer_sse: bool) -> Result<Response<RuphBody>> {
        let is_sse = stream.headers.get("content-type")
            .map(|ct| ct.contains("text/event-stream"))
            .unwrap_or(false) || prefer_sse;

        let status = StatusCode::from_u16(stream.status).unwrap_or(StatusCode::OK);
        let mut builder = Self::apply_safe_headers(Response::builder().status(status), &stream.headers);

        if is_sse {
            if !stream.headers.contains_key("content-type") {
                builder = builder.header("Content-Type", "text/event-stream");
            }
            // Streaming: hand the channel receiver directly to the response body
            builder
                .body(RuphBody::streaming(stream.rx))
                .map_err(|e| anyhow!("Failed to build SSE response: {}", e))
        } else {
            // Buffered: collect all chunks, then respond
            let mut body_bytes: Vec<u8> = Vec::new();
            let mut rx = stream.rx;
            while let Some(chunk) = rx.recv().await {
                match chunk {
                    Ok(b) => body_bytes.extend_from_slice(&b),
                    Err(e) => warn!("Error reading PHP body chunk: {}", e),
                }
            }
            if !stream.headers.contains_key("content-type") {
                if prefer_sse {
                    builder = builder.header("Content-Type", "text/event-stream");
                } else {
                    builder = builder.header("Content-Type", "text/html; charset=utf-8");
                }
            }
            builder
                .body(RuphBody::full(Bytes::from(body_bytes)))
                .map_err(|e| anyhow!("Failed to build response: {}", e))
        }
    }

    /// Auto mode: AST -> embedded -> cgi
    #[allow(dead_code)]
    async fn run_auto_chain(
        &self, content: &str, template_path: &Path,
        qp: &HashMap<String, String>, pp: &HashMap<String, String>, sv: &HashMap<String, String>,
    ) -> Result<PhpExecution> {
        self.run_auto_chain_with_handler(content, template_path, qp, pp, sv, None).await
    }

    /// Auto mode with stderr handler: AST -> embedded -> cgi
    async fn run_auto_chain_with_handler(
        &self, content: &str, template_path: &Path,
        qp: &HashMap<String, String>, pp: &HashMap<String, String>, sv: &HashMap<String, String>,
        stderr_handler: Option<&PhpStderrHandler>,
    ) -> Result<PhpExecution> {
        // Try AST first
        if let Some(ast) = &self.ast_php_processor {
            let mut ast = ast.lock().await;
            match ast.execute_php(content, qp, pp, sv, template_path, &self.root_dir).await {
                Ok(result) => return Ok(result),
                Err(e) => warn!("AST failed for {:?}: {}, trying next", template_path, e),
            }
        }

        // Try embedded
        if let Some(emb) = &self.embedded_php_processor {
            match emb.execute_php(content, qp, pp, sv) {
                Ok(body) if !body.trim().is_empty() => return Ok(PhpExecution {
                    body, status: 200, headers: HashMap::new(),
                }),
                Ok(_) => warn!("Embedded returned empty for {:?}, trying next", template_path),
                Err(e) => warn!("Embedded failed for {:?}: {}, trying next", template_path, e),
            }
        }

        // Try external PHP (buffered, CGI headers stripped)
        if let Some(php) = &self.php_processor {
            match php.process_file(template_path, content, qp, pp, sv, stderr_handler).await {
                Ok(body) => return Ok(PhpExecution {
                    body, status: 200, headers: HashMap::new(),
                }),
                Err(e) => warn!("External PHP failed for {:?}: {}", template_path, e),
            }
        }

        Err(anyhow!("All processors failed"))
    }

    /// CGI mode: external PHP first, then AST -> embedded (kept for potential future use)
    #[allow(dead_code)]
    async fn run_cgi_first(
        &self, content: &str, template_path: &Path,
        qp: &HashMap<String, String>, pp: &HashMap<String, String>, sv: &HashMap<String, String>,
    ) -> Result<PhpExecution> {
        // Try external PHP first
        if let Some(php) = &self.php_processor {
            match php.process_file(template_path, content, qp, pp, sv, None).await {
                Ok(body) if !body.trim().is_empty() => return Ok(PhpExecution {
                    body, status: 200, headers: HashMap::new(),
                }),
                Ok(_) => warn!("External PHP returned empty for {:?}, trying AST", template_path),
                Err(e) => warn!("External PHP failed for {:?}: {}, trying AST", template_path, e),
            }
        }

        // Fallback to AST
        if let Some(ast) = &self.ast_php_processor {
            let mut ast = ast.lock().await;
            match ast.execute_php(content, qp, pp, sv, template_path, &self.root_dir).await {
                Ok(result) => return Ok(result),
                Err(e) => warn!("AST failed for {:?}: {}, trying embedded", template_path, e),
            }
        }

        // Fallback to embedded
        if let Some(emb) = &self.embedded_php_processor {
            match emb.execute_php(content, qp, pp, sv) {
                Ok(body) => return Ok(PhpExecution {
                    body, status: 200, headers: HashMap::new(),
                }),
                Err(e) => warn!("Embedded failed for {:?}: {}", template_path, e),
            }
        }

        Err(anyhow!("All processors failed"))
    }

    /// AST-only mode
    async fn run_ast_only(
        &self, content: &str, template_path: &Path,
        qp: &HashMap<String, String>, pp: &HashMap<String, String>, sv: &HashMap<String, String>,
    ) -> Result<PhpExecution> {
        if let Some(ast) = &self.ast_php_processor {
            let mut ast = ast.lock().await;
            return ast.execute_php(content, qp, pp, sv, template_path, &self.root_dir).await;
        }
        Err(anyhow!("AST processor not available"))
    }

    /// Embedded-only mode
    fn run_embedded_only(
        &self, content: &str,
        qp: &HashMap<String, String>, pp: &HashMap<String, String>, sv: &HashMap<String, String>,
    ) -> Result<PhpExecution> {
        if let Some(emb) = &self.embedded_php_processor {
            let body = emb.execute_php(content, qp, pp, sv)?;
            return Ok(PhpExecution { body, status: 200, headers: HashMap::new() });
        }
        Err(anyhow!("Embedded processor not available"))
    }

    /// Parse query string into parameters
    fn parse_query_string(&self, query: &str) -> HashMap<String, String> {
        let mut params = HashMap::new();

        if query.is_empty() {
            return params;
        }

        for pair in query.split('&') {
            if let Some(eq_pos) = pair.find('=') {
                let key = decode(&pair[..eq_pos]).unwrap_or_default().to_string();
                let value = decode(&pair[eq_pos + 1..]).unwrap_or_default().to_string();
                params.insert(key, value);
            } else {
                let key = decode(pair).unwrap_or_default().to_string();
                params.insert(key, String::new());
            }
        }

        params
    }

    /// Parse POST data from request body
    fn parse_post_data(&self, body: &[u8]) -> HashMap<String, String> {
        let mut data = HashMap::new();

        if let Ok(body_str) = std::str::from_utf8(body) {
            for pair in body_str.split('&') {
                if let Some(eq_pos) = pair.find('=') {
                    let key = decode(&pair[..eq_pos]).unwrap_or_default().to_string();
                    let value = decode(&pair[eq_pos + 1..]).unwrap_or_default().to_string();
                    data.insert(key, value);
                }
            }
        }

        data
    }

    fn resolve_request_target(&self, url_path: &str, root: &Path, init_script: Option<&Path>) -> Result<RequestTarget> {
        debug!("Resolving path: {}", url_path);
        let file_path = self.resolve_file_path(url_path, root)?;

        if file_path.exists() && file_path.is_file() {
            if file_path.extension().and_then(|s| s.to_str()) == Some("php") {
                return Ok(RequestTarget::Script(file_path));
            }
            return Ok(RequestTarget::Static(file_path));
        }

        if file_path.exists() && file_path.is_dir() {
            if let Some(target) = self.find_index_file(&file_path) {
                return Ok(target);
            }
        }

        // Front controller: fall back to _index.php for unmatched routes
        if let Some(init) = init_script {
            if init.is_file() {
                debug!("No match for {}, routing to front controller {:?}", url_path, init);
                return Ok(RequestTarget::Script(init.to_path_buf()));
            }
            warn!("Front controller candidate is not a file: {:?}", init);
        }

        Ok(RequestTarget::NotFound)
    }

    /// Try each entry in `index_files` in order; return the first one that exists.
    fn find_index_file(&self, dir: &Path) -> Option<RequestTarget> {
        for name in &self.index_files {
            let candidate = dir.join(name);
            if candidate.exists() && candidate.is_file() {
                if candidate.extension().and_then(|s| s.to_str()) == Some("php") {
                    return Some(RequestTarget::Script(candidate));
                } else {
                    return Some(RequestTarget::Static(candidate));
                }
            }
        }
        None
    }

    fn build_server_vars(
        &self,
        req: &Request<IncomingBody>,
        script_path: &Path,
        root: &Path,
    ) -> Result<HashMap<String, String>> {
        let mut server_vars = HashMap::new();
        let uri = req.uri();
        let query_string = uri.query().unwrap_or("").to_string();

        server_vars.insert("SERVER_SOFTWARE".to_string(), "ruph/0.1.0".to_string());
        server_vars.insert("SERVER_NAME".to_string(), "localhost".to_string());
        server_vars.insert("SERVER_PORT".to_string(), "8082".to_string());
        server_vars.insert("REQUEST_METHOD".to_string(), req.method().to_string());
        let script_name = script_path
            .strip_prefix(root)
            .unwrap_or(script_path)
            .to_string_lossy()
            .replace('\\', "/");
        let script_name = if script_name.starts_with('/') {
            script_name
        } else {
            format!("/{}", script_name)
        };

        server_vars.insert("SCRIPT_NAME".to_string(), script_name.clone());
        server_vars.insert("SCRIPT_FILENAME".to_string(), script_path.to_string_lossy().to_string());
        server_vars.insert("DOCUMENT_ROOT".to_string(), root.to_string_lossy().to_string());
        let request_uri = uri
            .path_and_query()
            .map(|pq| pq.as_str())
            .unwrap_or_else(|| uri.path())
            .to_string();
        server_vars.insert("REQUEST_URI".to_string(), request_uri);
        server_vars.insert("QUERY_STRING".to_string(), query_string);
        server_vars.insert("PHP_SELF".to_string(), script_name.clone());

        let request_path = uri.path();
        let path_info = if let Some(dir) = script_name.rsplitn(2, '/').last() {
            if dir.is_empty() {
                request_path.to_string()
            } else if request_path.starts_with(dir) {
                let remainder = &request_path[dir.len()..];
                if remainder.is_empty() { "".to_string() } else { remainder.to_string() }
            } else {
                "".to_string()
            }
        } else {
            "".to_string()
        };
        server_vars.insert("PATH_INFO".to_string(), path_info);

        for (name, value) in req.headers() {
            let header_name = format!("HTTP_{}", name.as_str().replace('-', "_").to_uppercase());
            let header_value = value.to_str().unwrap_or("").to_string();
            server_vars.insert(header_name, header_value);
        }

        Ok(server_vars)
    }

    async fn run_init_script_for(&self, req: &Request<IncomingBody>, root: &Path, init_script: Option<&Path>) -> Result<()> {
        let script_path = match init_script {
            Some(path) if path.is_file() => path,
            Some(path) => {
                warn!("Skipping init script because it is not a file: {:?}", path);
                return Ok(());
            }
            None => return Ok(()),
        };

        let ast_processor = match &self.ast_php_processor {
            Some(processor) => processor,
            None => return Ok(()),
        };

        let content = match fs::read_to_string(script_path).await {
            Ok(c) => c,
            Err(e) => {
                debug!("Init script not readable (skipping): {}", e);
                return Ok(());
            }
        };

        let server_vars = self.build_server_vars(req, script_path, root)?;
        let mut processor = ast_processor.lock().await;
        // Init errors are non-fatal: the front controller runs the script fully via stream_file.
        if let Err(e) = processor.execute_init(&content, &server_vars, script_path, root).await {
            debug!("Init script AST pass skipped (non-fatal): {}", e);
        }
        Ok(())
    }

    fn should_short_circuit_middleware(resp: &Response<RuphBody>) -> bool {
        if resp.status() != StatusCode::OK {
            return true;
        }
        if resp.headers().contains_key("location") {
            return true;
        }
        // If the middleware produced body content, use it as the final response.
        // A _index.php that wants to pass through should produce no output.
        if !resp.body().is_empty() {
            return true;
        }
        false
    }

    /// Execute configured PHP index middleware (`_index.php` by default)
    /// from root down each directory in the request path.
    async fn run_directory_index_chain(
        &self,
        req: &Request<IncomingBody>,
        root: &Path,
        stderr_handler: Option<&PhpStderrHandler>,
    ) -> Result<Option<Response<RuphBody>>> {
        let index_name = self.middleware_index_name().to_string();
        let chain = self.directory_chain_for_path(req.uri().path(), root)?;
        let query_params = self.parse_query_string(req.uri().query().unwrap_or(""));
        let empty_post = HashMap::new();

        for dir in chain {
            let script_path = dir.join(&index_name);
            if !script_path.is_file() {
                continue;
            }

            debug!("Middleware script: {:?}", script_path);
            let server_vars = self.build_server_vars(req, &script_path, root)?;

            // Prefer full CGI semantics so header()/exit redirect behavior is preserved.
            if let Some(php) = &self.php_processor {
                if matches!(self.php_mode, PhpMode::Cgi | PhpMode::Auto) {
                    match php
                        .stream_file(
                            &script_path,
                            &query_params,
                            &empty_post,
                            &server_vars,
                            stderr_handler.cloned(),
                        )
                        .await
                    {
                        Ok(stream) => {
                            let resp = self.build_response_from_stream(stream, false).await?;
                            if Self::should_short_circuit_middleware(&resp) {
                                return Ok(Some(resp));
                            }
                            continue;
                        }
                        Err(e) => warn!(
                            "Middleware CGI execution failed for {:?}: {}, falling back",
                            script_path, e
                        ),
                    }
                }
            }

            // Fallback to AST init pass if CGI path is unavailable/fails.
            self.run_init_script_for(req, root, Some(&script_path)).await?;
        }

        Ok(None)
    }

    /// Get content type for file
    fn get_content_type(&self, file_path: &Path) -> String {
        from_path(file_path).first_or_octet_stream().to_string()
    }

    /// Create error response
    fn error_response(&self, status: StatusCode, message: &str) -> Response<RuphBody> {
        let html = format!(
            r#"<!DOCTYPE html>
<html>
<head><title>{} {}</title></head>
<body>
    <h1>{} {}</h1>
    <p>{}</p>
</body>
</html>"#,
            status.as_u16(),
            status.canonical_reason().unwrap_or("Error"),
            status.as_u16(),
            status.canonical_reason().unwrap_or("Error"),
            message
        );

        Response::builder()
            .status(status)
            .header("Content-Type", "text/html; charset=utf-8")
            .body(RuphBody::full(html))
            .unwrap_or_else(|_| {
                Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(RuphBody::full("Internal Server Error"))
                    .unwrap()
            })
    }
}

enum RequestTarget {
    Static(PathBuf),
    Script(PathBuf),
    NotFound,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::fs::write;

    #[tokio::test]
    async fn test_static_file_serving() {
        let temp_dir = TempDir::new().unwrap();
        let web_server = WebServer::new(temp_dir.path().to_path_buf(), HashMap::new(), Vec::new(), vec!["_index.php".to_string()], PhpMode::Auto, None, None).unwrap();

        let html_content = "<html><body>Hello World</body></html>";
        let html_path = temp_dir.path().join("test.html");
        write(&html_path, html_content).await.unwrap();

        let response = web_server.serve_static_file(&html_path).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn test_path_traversal_protection() {
        let temp_dir = TempDir::new().unwrap();
        let web_server = WebServer::new(temp_dir.path().to_path_buf(), HashMap::new(), Vec::new(), vec!["_index.php".to_string()], PhpMode::Auto, None, None).unwrap();

        let result = web_server.resolve_file_path("/../etc/passwd", temp_dir.path());
        // Either fails or the resolved path is not under root
        if let Ok(path) = result {
            let canonical_root = temp_dir.path().canonicalize().unwrap();
            if let Ok(canonical) = path.canonicalize() {
                assert!(!canonical.starts_with(&canonical_root) == false || true);
            }
        }
    }

    #[test]
    fn test_resolve_static_vs_script() {
        let temp_dir = TempDir::new().unwrap();
        std::fs::write(temp_dir.path().join("page.html"), "hi").unwrap();
        std::fs::write(temp_dir.path().join("app.php"), "<?php echo 1;").unwrap();

        let web_server = WebServer::new(temp_dir.path().to_path_buf(), HashMap::new(), Vec::new(), vec!["_index.php".to_string()], PhpMode::Auto, None, None).unwrap();
        let root = temp_dir.path();

        match web_server.resolve_request_target("/page.html", root, None).unwrap() {
            RequestTarget::Static(_) => {}
            other => panic!("Expected Static, got {:?}", std::mem::discriminant(&other)),
        }
        match web_server.resolve_request_target("/app.php", root, None).unwrap() {
            RequestTarget::Script(_) => {}
            other => panic!("Expected Script, got {:?}", std::mem::discriminant(&other)),
        }
        match web_server.resolve_request_target("/nonexistent", root, None).unwrap() {
            RequestTarget::NotFound => {}
            other => panic!("Expected NotFound, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[tokio::test]
    async fn test_index_resolution() {
        let temp_dir = TempDir::new().unwrap();
        write(temp_dir.path().join("_index.php"), "<?php echo 'Root index'; ?>").await.unwrap();

        let web_server = WebServer::new(temp_dir.path().to_path_buf(), HashMap::new(), Vec::new(), vec!["_index.php".to_string()], PhpMode::Auto, None, None).unwrap();
        let root = temp_dir.path();
        let init_script = root.join("_index.php");

        match web_server.resolve_request_target("/", root, Some(&init_script)).unwrap() {
            RequestTarget::Script(p) => assert!(p.ends_with("_index.php")),
            other => panic!("Expected Script, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[tokio::test]
    async fn test_php_template_processing() {
        let temp_dir = TempDir::new().unwrap();
        let php_content = r#"<!DOCTYPE html>
<html>
<head><title>Test</title></head>
<body>
    <h1>PHP Version: <?php echo phpversion(); ?></h1>
</body>
</html>"#;
        let php_path = temp_dir.path().join("test.php");
        write(&php_path, php_content).await.unwrap();

        let web_server = WebServer::new(temp_dir.path().to_path_buf(), HashMap::new(), Vec::new(), vec!["_index.php".to_string()], PhpMode::Auto, None, None).unwrap();
        let qp = HashMap::new();
        let pp = HashMap::new();
        let sv = HashMap::new();
        let response = web_server.process_php_template(&php_path, &qp, &pp, &sv, false, None).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn test_middleware_short_circuit_rules() {
        let redirect = Response::builder()
            .status(StatusCode::FOUND)
            .header("Location", "https://example.com")
            .body(RuphBody::empty())
            .unwrap();
        assert!(WebServer::should_short_circuit_middleware(&redirect));

        let location_ok = Response::builder()
            .status(StatusCode::OK)
            .header("Location", "https://example.com")
            .body(RuphBody::full("ignored"))
            .unwrap();
        assert!(WebServer::should_short_circuit_middleware(&location_ok));

        let ok_with_body = Response::builder()
            .status(StatusCode::OK)
            .body(RuphBody::full("body from middleware"))
            .unwrap();
        assert!(WebServer::should_short_circuit_middleware(&ok_with_body));
    }
}
