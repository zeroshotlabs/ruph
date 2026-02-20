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

/// Web server handler for HTTP requests
pub struct WebServer {
    /// Root directory for serving files
    pub root_dir: PathBuf,
    /// AST-based PHP processor (preferred)
    ast_php_processor: Option<Mutex<AstPhpProcessor>>,
    /// Embedded PHP processor (fallback)
    embedded_php_processor: Option<EmbeddedPhpProcessor>,
    /// Optional init script at docroot/_index.php
    init_script: Option<PathBuf>,
}

impl WebServer {
    /// Create a new web server instance
    pub fn new(root_dir: PathBuf) -> Result<Self> {
        // Try to initialize AST-based PHP processor first
        let ast_php_processor = match AstPhpProcessor::new() {
            Ok(processor) => {
                debug!("AST-based PHP processor initialized successfully");
                Some(Mutex::new(processor))
            }
            Err(e) => {
                warn!("Failed to initialize AST PHP processor: {}. Falling back to embedded processor.", e);
                None
            }
        };
        
        // Initialize embedded PHP processor as fallback
        let embedded_php_processor = match EmbeddedPhpProcessor::new() {
            Ok(processor) => {
                debug!("Embedded PHP processor initialized successfully");
                Some(processor)
            }
            Err(e) => {
                warn!("Failed to initialize embedded PHP processor: {}. PHP files will be served as static files.", e);
                None
            }
        };
        
        if ast_php_processor.is_none() && embedded_php_processor.is_none() {
            warn!("No PHP processors available. PHP files will be served as static content.");
        }
        
        let init_script = {
            let candidate = root_dir.join("_index.php");
            if candidate.exists() {
                Some(candidate)
            } else {
                None
            }
        };

        Ok(Self {
            root_dir,
            ast_php_processor,
            embedded_php_processor,
            init_script,
        })
    }

    /// Handle HTTP web requests (non-MCP)
    pub async fn handle_request(&self, req: Request<IncomingBody>) -> Result<Response<Full<Bytes>>> {
        info!("HTTP {} {}", req.method(), req.uri());
        self.run_init_script(&req).await?;

        let method = req.method();
        let uri = req.uri();
        let path = uri.path();

        debug!("Web request: {} {}", method, path);

        // Security: Prevent path traversal attacks
        if path.contains("..") || path.contains("\\") {
            return Ok(self.error_response(StatusCode::FORBIDDEN, "Access denied"));
        }

        match method {
            &Method::GET => self.handle_get_request(req).await,
            &Method::POST => self.handle_post_request(req).await,
            &Method::HEAD => self.handle_head_request(req).await,
            _ => Ok(self.error_response(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed")),
        }
    }

    /// Handle GET requests
    async fn handle_get_request(&self, req: Request<IncomingBody>) -> Result<Response<Full<Bytes>>> {
        let uri = req.uri();
        let path = uri.path();
        let query = uri.query();

        match self.resolve_request_target(path)? {
            RequestTarget::Static(file_path) => self.serve_static_file(&file_path).await,
            RequestTarget::Script(script_path) => {
                let query_params = self.parse_query_string(query.unwrap_or(""));
                let server_vars = self.build_server_vars(&req, &script_path)?;
                self.process_php_template(&script_path, &query_params, &HashMap::new(), &server_vars).await
            }
            RequestTarget::NotFound => Ok(self.error_response(StatusCode::NOT_FOUND, "File not found")),
        }
    }

    /// Handle POST requests
    async fn handle_post_request(&self, req: Request<IncomingBody>) -> Result<Response<Full<Bytes>>> {
        let uri = req.uri().clone();
        let path = uri.path();
        let target = self.resolve_request_target(path)?;
        let script_path = match target {
            RequestTarget::Script(path) => path,
            RequestTarget::Static(_) => {
                return Ok(self.error_response(StatusCode::METHOD_NOT_ALLOWED, "POST not allowed for this file type"));
            }
            RequestTarget::NotFound => {
                return Ok(self.error_response(StatusCode::NOT_FOUND, "File not found"));
            }
        };

        let server_vars = self.build_server_vars(&req, &script_path)?;

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
    async fn handle_head_request(&self, req: Request<IncomingBody>) -> Result<Response<Full<Bytes>>> {
        let uri = req.uri();
        let path = uri.path();

        match self.resolve_request_target(path)? {
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

    /// Resolve file path from URL path
    fn resolve_file_path(&self, url_path: &str) -> Result<PathBuf> {
        let decoded_path = decode(url_path).map_err(|_| anyhow!("Invalid URL encoding"))?;
        let clean_path = decoded_path.trim_start_matches('/');
        
        let file_path = if clean_path.is_empty() {
            self.root_dir.clone()
        } else {
            self.root_dir.join(clean_path)
        };

        // Ensure the resolved path is within the root directory
        let canonical_root = self.root_dir.canonicalize()
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

    /// Process PHP-like template
    async fn process_php_template(
        &self,
        template_path: &Path,
        query_params: &HashMap<String, String>,
        post_params: &HashMap<String, String>,
        server_vars: &HashMap<String, String>,
    ) -> Result<Response<Full<Bytes>>> {
        debug!("Processing PHP template: {:?}", template_path);
        info!("Executing PHP template: {:?}", template_path);
    
        // Check if any PHP processor is available
        if self.ast_php_processor.is_none() && self.embedded_php_processor.is_none() {
            warn!("No PHP processors available, serving PHP file as static content");
            return self.serve_static_file(template_path).await;
        }
    
        // Read the PHP file content
        let content = match fs::read_to_string(template_path).await {
            Ok(content) => content,
            Err(_) => return Ok(self.error_response(StatusCode::INTERNAL_SERVER_ERROR, "Cannot read PHP file")),
        };
    
        // Process the template - try AST processor first, then fall back to embedded processor
        let output = if let Some(ast_processor) = &self.ast_php_processor {
            let mut ast_processor = ast_processor.lock().await;
            debug!("Using AST-based PHP processor");
            match ast_processor.execute_php(
                &content,
                query_params,
                post_params,
                server_vars,
                template_path,
                &self.root_dir,
            ).await {
                Ok(mut result) => {
                    if result.body.trim().is_empty() {
                        if let Some(embedded_processor) = &self.embedded_php_processor {
                            warn!("AST processor returned empty output for {:?}; trying embedded processor", template_path);
                            match embedded_processor.execute_php(&content, query_params, post_params, &server_vars) {
                                Ok(fallback) => {
                                    if !fallback.trim().is_empty() {
                                        result.body = fallback;
                                    }
                                }
                                Err(e2) => {
                                    warn!("Embedded PHP processor failed after empty AST output: {}", e2);
                                }
                            }
                        }
                    }
                    result
                },
                Err(e) => {
                    warn!("AST PHP processor failed: {}. Trying embedded processor.", e);
                    if let Some(embedded_processor) = &self.embedded_php_processor {
                        debug!("Falling back to embedded PHP processor");
                        match embedded_processor.execute_php(&content, query_params, post_params, &server_vars) {
                            Ok(result) => PhpExecution {
                                body: result,
                                status: StatusCode::OK.as_u16(),
                                headers: HashMap::new(),
                            },
                            Err(e2) => {
                                error!("Both PHP processors failed. AST: {}, Embedded: {}", e, e2);
                                return Ok(self.error_response(StatusCode::INTERNAL_SERVER_ERROR,
                                    &format!("Template processing error: {}", e2)));
                            }
                        }
                    } else {
                        error!("AST PHP processor failed and no embedded processor available: {}", e);
                        return Ok(self.error_response(StatusCode::INTERNAL_SERVER_ERROR,
                            &format!("Template processing error: {}", e)));
                    }
                }
            }
        } else if let Some(embedded_processor) = &self.embedded_php_processor {
            debug!("Using embedded PHP processor");
            match embedded_processor.execute_php(&content, query_params, post_params, &server_vars) {
                Ok(result) => PhpExecution {
                    body: result,
                    status: StatusCode::OK.as_u16(),
                    headers: HashMap::new(),
                },
                Err(e) => {
                    error!("Embedded PHP processor failed: {}", e);
                    return Ok(self.error_response(StatusCode::INTERNAL_SERVER_ERROR,
                        &format!("Template processing error: {}", e)));
                }
            }
        } else {
            error!("No PHP processors available");
            return Ok(self.error_response(StatusCode::INTERNAL_SERVER_ERROR,
                "No PHP processors available"));
        };
    
        let mut response = Response::builder()
            .status(StatusCode::from_u16(output.status).unwrap_or(StatusCode::OK))
            .header("Content-Type", "text/html; charset=utf-8");

        for (name, value) in &output.headers {
            response = response.header(name, value);
        }

        info!("PHP response body length: {}", output.body.len());

        response
            .body(Full::new(Bytes::from(output.body)))
            .map_err(|e| anyhow!("Failed to build response: {}", e))
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

    fn resolve_request_target(&self, url_path: &str) -> Result<RequestTarget> {
        debug!("Resolving path: {}", url_path);
        let file_path = self.resolve_file_path(url_path)?;

        if file_path.exists() && file_path.is_file() {
            if file_path.extension().and_then(|s| s.to_str()) == Some("php") {
                return Ok(RequestTarget::Script(file_path));
            }
            return Ok(RequestTarget::Static(file_path));
        }

        if file_path.exists() && file_path.is_dir() {
            let script_path = self.find_index_script(&file_path)?;
            if let Some(script_path) = script_path {
                return Ok(RequestTarget::Script(script_path));
            }
        }

        warn!("No static file or _index.php found for {}", url_path);

        Ok(RequestTarget::NotFound)
    }

    fn find_index_script(&self, start_dir: &Path) -> Result<Option<PathBuf>> {
        let candidate = start_dir.join("_index.php");
        if candidate.exists() {
            return Ok(Some(candidate));
        }
        Ok(None)
    }

    fn build_server_vars(
        &self,
        req: &Request<IncomingBody>,
        script_path: &Path,
    ) -> Result<HashMap<String, String>> {
        let mut server_vars = HashMap::new();
        let uri = req.uri();
        let query_string = uri.query().unwrap_or("").to_string();

        server_vars.insert("SERVER_SOFTWARE".to_string(), "MCP-Filesystem-Server/0.1.0".to_string());
        server_vars.insert("SERVER_NAME".to_string(), "localhost".to_string());
        server_vars.insert("SERVER_PORT".to_string(), "8082".to_string());
        server_vars.insert("REQUEST_METHOD".to_string(), req.method().to_string());
        let script_name = script_path
            .strip_prefix(&self.root_dir)
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
        server_vars.insert("DOCUMENT_ROOT".to_string(), self.root_dir.to_string_lossy().to_string());
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

    async fn run_init_script(&self, req: &Request<IncomingBody>) -> Result<()> {
        let script_path = match &self.init_script {
            Some(path) => path.clone(),
            None => return Ok(()),
        };

        let ast_processor = match &self.ast_php_processor {
            Some(processor) => processor,
            None => return Ok(()),
        };

        let content = fs::read_to_string(&script_path).await
            .map_err(|_| anyhow!("Cannot read init PHP file"))?;

        let server_vars = self.build_server_vars(req, &script_path)?;
        let mut processor = ast_processor.lock().await;
        processor.execute_init(&content, &server_vars, &script_path, &self.root_dir).await?;
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

    fn make_request(method: &str, uri: &str) -> Request<IncomingBody> {
        Request::builder()
            .method(method)
            .uri(uri)
            .body(IncomingBody::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn test_static_file_serving() {
        let temp_dir = TempDir::new().unwrap();
        let web_server = WebServer::new(temp_dir.path().to_path_buf()).unwrap();
        
        // Create a test HTML file
        let html_content = "<html><body>Hello World</body></html>";
        let html_path = temp_dir.path().join("test.html");
        write(&html_path, html_content).await.unwrap();
        
        // Test serving the file
        let response = web_server.handle_request(make_request("GET", "/test.html")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_path_traversal_protection() {
        let temp_dir = TempDir::new().unwrap();
        let web_server = WebServer::new(temp_dir.path().to_path_buf()).unwrap();
        
        // Test path traversal attempt
        let response = web_server.handle_request(make_request("GET", "/../etc/passwd")).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_index_resolution() {
        let temp_dir = TempDir::new().unwrap();
        let web_server = WebServer::new(temp_dir.path().to_path_buf()).unwrap();
        
        // Create a root _index.php for fallback routing
        let php_path = temp_dir.path().join("_index.php");
        write(&php_path, "<?php echo \"Root index\"; ?>").await.unwrap();

        let response = web_server.handle_request(make_request("GET", "/missing/path")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_php_template_processing() {
        let temp_dir = TempDir::new().unwrap();
        let web_server = WebServer::new(temp_dir.path().to_path_buf()).unwrap();
        
        // Create a test PHP template file
        let php_content = r#"<!DOCTYPE html>
<html>
<head><title>Test</title></head>
<body>
    <h1>PHP Version: <?php echo phpversion(); ?></h1>
    <p>Current time: <?php echo date('Y-m-d H:i:s'); ?></p>
</body>
</html>"#;
        let php_path = temp_dir.path().join("test.php");
        write(&php_path, php_content).await.unwrap();
        
        // Test processing the PHP template
        let response = web_server.handle_request(make_request("GET", "/test.php")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
