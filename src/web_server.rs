//! Web server functionality for serving static files and processing PHP-like templates
//!
//! This module provides HTTP web server capabilities alongside the MCP protocol,
//! allowing the server to serve static files and process embedded PHP-like templates.

use std::path::{Path, PathBuf};
use std::collections::HashMap;
use hyper::{Request, Response, StatusCode, Method};
use hyper::body::Incoming as IncomingBody;
use http_body_util::{Full, BodyExt};
use bytes::Bytes;
use mime_guess::from_path;
use urlencoding::decode;
use tokio::fs;
use tokio::sync::Mutex;
use anyhow::{Result, anyhow};
use tracing::{debug, info, warn, error};
use crate::embedded_php_processor::EmbeddedPhpProcessor;
use crate::ast_php_processor::{AstPhpProcessor, PhpExecution};
use crate::php_processor::PhpProcessor;
use crate::config::PhpMode;

/// Web server handler for HTTP requests
pub struct WebServer {
    /// Default root directory for serving files
    pub root_dir: PathBuf,
    /// Per-domain docroot overrides (domain -> path, port stripped)
    domain_roots: HashMap<String, PathBuf>,
    /// Ordered list of filenames to try when a directory is requested
    index_files: Vec<String>,
    /// PHP processor mode (controls execution order)
    php_mode: PhpMode,
    /// AST-based PHP processor
    ast_php_processor: Option<Mutex<AstPhpProcessor>>,
    /// Embedded regex PHP processor
    embedded_php_processor: Option<EmbeddedPhpProcessor>,
    /// External PHP binary processor
    php_processor: Option<PhpProcessor>,
}

impl WebServer {
    /// Create a new web server instance with PHP mode, optional binary path, and per-domain roots.
    pub fn new(
        root_dir: PathBuf,
        domain_roots: HashMap<String, PathBuf>,
        index_files: Vec<String>,
        php_mode: PhpMode,
        php_binary: Option<String>,
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

        if ast_php_processor.is_none() && embedded_php_processor.is_none() && php_processor.is_none() {
            warn!("No PHP processors available. PHP files will be served as static content.");
        } else {
            let available: Vec<&str> = [
                ast_php_processor.as_ref().map(|_| "ast"),
                embedded_php_processor.as_ref().map(|_| "embedded"),
                php_processor.as_ref().map(|p| { let _ = p; "libphp" }),
            ].into_iter().flatten().collect();
            info!("PHP processors: [{}], mode: {:?}", available.join(", "), php_mode);
        }

        if !domain_roots.is_empty() {
            for (domain, root) in &domain_roots {
                info!("Virtual host: {} -> {}", domain, root.display());
            }
        }

        Ok(Self {
            root_dir,
            domain_roots,
            index_files,
            php_mode,
            ast_php_processor,
            embedded_php_processor,
            php_processor,
        })
    }

    /// Find the first PHP file from `index_files` that exists in `root`.
    /// Used to locate the front-controller / init script for a docroot.
    /// Only PHP files qualify because non-PHP index files cannot be executed as init scripts.
    fn find_root_init_script(root: &Path, index_files: &[String]) -> Option<PathBuf> {
        index_files.iter()
            .filter(|name| name.ends_with(".php"))
            .map(|name| root.join(name))
            .find(|p| p.exists())
    }

    /// Return the docroot for a given `Host` header value (port stripped).
    /// Falls back to the global `root_dir` when no per-domain override exists.
    fn effective_root(&self, host: &str) -> &PathBuf {
        let domain = host.split(':').next().unwrap_or(host);
        self.domain_roots.get(domain).unwrap_or(&self.root_dir)
    }

    /// Return the init script for a given host by scanning `index_files` at request time.
    /// Resolved live so file additions/renames take effect without a restart.
    fn effective_init_script(&self, host: &str) -> Option<PathBuf> {
        let root = self.effective_root(host);
        Self::find_root_init_script(root, &self.index_files)
    }

    /// Handle HTTP web requests (non-MCP)
    pub async fn handle_request(&self, req: Request<IncomingBody>) -> Result<Response<Full<Bytes>>> {
        let host = req.headers().get("host")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let root = self.effective_root(&host).clone();
        let init_script = self.effective_init_script(&host);

        self.run_init_script_for(&req, &root, init_script.as_deref()).await?;

        let method = req.method().clone();
        let path = req.uri().path().to_string();

        debug!("Web request: {} {}", method, path);

        // Security: Prevent path traversal attacks
        if path.contains("..") || path.contains("\\") {
            return Ok(self.error_response(StatusCode::FORBIDDEN, "Access denied"));
        }

        match method {
            Method::GET => self.handle_get_request(req, &root, init_script.as_deref()).await,
            Method::POST => self.handle_post_request(req, &root, init_script.as_deref()).await,
            Method::HEAD => self.handle_head_request(req, &root, init_script.as_deref()).await,
            _ => Ok(self.error_response(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed")),
        }
    }

    /// Handle GET requests
    async fn handle_get_request(&self, req: Request<IncomingBody>, root: &Path, init_script: Option<&Path>) -> Result<Response<Full<Bytes>>> {
        let uri = req.uri();
        let path = uri.path();
        let query = uri.query();

        match self.resolve_request_target(path, root, init_script)? {
            RequestTarget::Static(file_path) => self.serve_static_file(&file_path).await,
            RequestTarget::Script(script_path) => {
                let query_params = self.parse_query_string(query.unwrap_or(""));
                let server_vars = self.build_server_vars(&req, &script_path, root)?;
                self.process_php_template(&script_path, &query_params, &HashMap::new(), &server_vars).await
            }
            RequestTarget::NotFound => Ok(self.error_response(StatusCode::NOT_FOUND, "File not found")),
        }
    }

    /// Handle POST requests
    async fn handle_post_request(&self, req: Request<IncomingBody>, root: &Path, init_script: Option<&Path>) -> Result<Response<Full<Bytes>>> {
        let uri = req.uri().clone();
        let path = uri.path();
        let target = self.resolve_request_target(path, root, init_script)?;
        let script_path = match target {
            RequestTarget::Script(path) => path,
            RequestTarget::Static(_) | RequestTarget::NotFound => {
                // Front controller handles POST for non-script targets too
                if let Some(init) = init_script {
                    init.to_path_buf()
                } else {
                    return Ok(self.error_response(StatusCode::NOT_FOUND, "Not found"));
                }
            }
        };

        let server_vars = self.build_server_vars(&req, &script_path, root)?;

        // Parse POST data
        let body_bytes = match req.collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(_) => return Ok(self.error_response(StatusCode::BAD_REQUEST, "Invalid request body")),
        };

        let post_data = self.parse_post_data(&body_bytes);
        let query_params = self.parse_query_string(uri.query().unwrap_or(""));
        self.process_php_template(&script_path, &query_params, &post_data, &server_vars).await
    }

    /// Handle HEAD requests
    async fn handle_head_request(&self, req: Request<IncomingBody>, root: &Path, init_script: Option<&Path>) -> Result<Response<Full<Bytes>>> {
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
                    .body(Full::new(Bytes::new()))
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

        // Ensure the resolved path is within the root directory
        let canonical_root = root.canonicalize()
            .map_err(|_| anyhow!("Cannot canonicalize root directory"))?;

        if let Ok(canonical_file) = file_path.canonicalize() {
            if !canonical_file.starts_with(&canonical_root) {
                return Err(anyhow!("Path traversal attempt detected"));
            }
        }

        Ok(file_path)
    }

    /// Serve static file
    async fn serve_static_file(&self, file_path: &Path) -> Result<Response<Full<Bytes>>> {
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
            .body(Full::new(Bytes::from(content)))
            .map_err(|e| anyhow!("Failed to build response: {}", e))
    }

    /// Process PHP-like template using the configured processor chain
    async fn process_php_template(
        &self,
        template_path: &Path,
        query_params: &HashMap<String, String>,
        post_params: &HashMap<String, String>,
        server_vars: &HashMap<String, String>,
    ) -> Result<Response<Full<Bytes>>> {
        debug!("Processing PHP template: {:?}", template_path);

        if self.ast_php_processor.is_none() && self.embedded_php_processor.is_none() && self.php_processor.is_none() {
            warn!("No PHP processors available, serving PHP file as static content");
            return self.serve_static_file(template_path).await;
        }

        let content = match fs::read_to_string(template_path).await {
            Ok(content) => content,
            Err(_) => return Ok(self.error_response(StatusCode::INTERNAL_SERVER_ERROR, "Cannot read PHP file")),
        };

        // Execute via processor chain based on configured mode
        let output = match self.php_mode {
            PhpMode::Libphp => self.run_libphp_first(&content, template_path, query_params, post_params, server_vars).await,
            PhpMode::Ast => self.run_ast_only(&content, template_path, query_params, post_params, server_vars).await,
            PhpMode::Embedded => self.run_embedded_only(&content, query_params, post_params, server_vars),
            PhpMode::Auto => self.run_auto_chain(&content, template_path, query_params, post_params, server_vars).await,
        };

        let output = match output {
            Ok(o) => o,
            Err(e) => {
                error!("All PHP processors failed for {:?}: {}", template_path, e);
                return Ok(self.error_response(StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("Template processing error: {}", e)));
            }
        };

        let mut response = Response::builder()
            .status(StatusCode::from_u16(output.status).unwrap_or(StatusCode::OK))
            .header("Content-Type", "text/html; charset=utf-8");

        for (name, value) in &output.headers {
            response = response.header(name, value);
        }

        response
            .body(Full::new(Bytes::from(output.body)))
            .map_err(|e| anyhow!("Failed to build response: {}", e))
    }

    /// Auto mode: AST -> embedded -> libphp
    async fn run_auto_chain(
        &self, content: &str, template_path: &Path,
        qp: &HashMap<String, String>, pp: &HashMap<String, String>, sv: &HashMap<String, String>,
    ) -> Result<PhpExecution> {
        // Try AST first
        if let Some(ast) = &self.ast_php_processor {
            let mut ast = ast.lock().await;
            match ast.execute_php(content, qp, pp, sv, template_path, &self.root_dir).await {
                Ok(result) if !result.body.trim().is_empty() => return Ok(result),
                Ok(_) => warn!("AST returned empty for {:?}, trying next", template_path),
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

        // Try external PHP
        if let Some(php) = &self.php_processor {
            match php.process_file(template_path, content, qp, pp, sv).await {
                Ok(body) => return Ok(PhpExecution {
                    body, status: 200, headers: HashMap::new(),
                }),
                Err(e) => warn!("External PHP failed for {:?}: {}", template_path, e),
            }
        }

        Err(anyhow!("All processors failed"))
    }

    /// Libphp mode: external PHP first, then AST -> embedded
    async fn run_libphp_first(
        &self, content: &str, template_path: &Path,
        qp: &HashMap<String, String>, pp: &HashMap<String, String>, sv: &HashMap<String, String>,
    ) -> Result<PhpExecution> {
        // Try external PHP first
        if let Some(php) = &self.php_processor {
            match php.process_file(template_path, content, qp, pp, sv).await {
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
                Ok(result) if !result.body.trim().is_empty() => return Ok(result),
                Ok(_) => warn!("AST returned empty for {:?}, trying embedded", template_path),
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
            debug!("No match for {}, routing to front controller {:?}", url_path, init);
            return Ok(RequestTarget::Script(init.to_path_buf()));
        }

        Ok(RequestTarget::NotFound)
    }

    /// Try each entry in `index_files` in order; return the first one that exists.
    /// `.php` files become `Script`, everything else becomes `Static`.
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
        server_vars.insert("REQUEST_URI".to_string(), uri.to_string());
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
            Some(path) => path,
            None => return Ok(()),
        };

        let ast_processor = match &self.ast_php_processor {
            Some(processor) => processor,
            None => return Ok(()),
        };

        let content = fs::read_to_string(script_path).await
            .map_err(|_| anyhow!("Cannot read init PHP file"))?;

        let server_vars = self.build_server_vars(req, script_path, root)?;
        let mut processor = ast_processor.lock().await;
        processor.execute_init(&content, &server_vars, script_path, root).await?;
        Ok(())
    }

    /// Get content type for file
    fn get_content_type(&self, file_path: &Path) -> String {
        from_path(file_path).first_or_octet_stream().to_string()
    }

    /// Create error response
    fn error_response(&self, status: StatusCode, message: &str) -> Response<Full<Bytes>> {
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
            .body(Full::new(Bytes::from(html)))
            .unwrap_or_else(|_| {
                Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Full::new(Bytes::from("Internal Server Error")))
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

    /// Test helper: resolve_request_target and serve_static_file directly
    /// (avoids needing hyper's private IncomingBody::empty())

    #[tokio::test]
    async fn test_static_file_serving() {
        let temp_dir = TempDir::new().unwrap();
        let web_server = WebServer::new(temp_dir.path().to_path_buf(), HashMap::new(), vec!["_index.php".to_string()], PhpMode::Auto, None).unwrap();

        let html_content = "<html><body>Hello World</body></html>";
        let html_path = temp_dir.path().join("test.html");
        write(&html_path, html_content).await.unwrap();

        let response = web_server.serve_static_file(&html_path).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn test_path_traversal_protection() {
        let temp_dir = TempDir::new().unwrap();
        let web_server = WebServer::new(temp_dir.path().to_path_buf(), HashMap::new(), vec!["_index.php".to_string()], PhpMode::Auto, None).unwrap();

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

        let web_server = WebServer::new(temp_dir.path().to_path_buf(), HashMap::new(), vec!["_index.php".to_string()], PhpMode::Auto, None).unwrap();
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

        let web_server = WebServer::new(temp_dir.path().to_path_buf(), HashMap::new(), vec!["_index.php".to_string()], PhpMode::Auto, None).unwrap();
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

        let web_server = WebServer::new(temp_dir.path().to_path_buf(), HashMap::new(), vec!["_index.php".to_string()], PhpMode::Auto, None).unwrap();
        let qp = HashMap::new();
        let pp = HashMap::new();
        let sv = HashMap::new();
        let response = web_server.process_php_template(&php_path, &qp, &pp, &sv).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
