//! AST-based PHP processor using tree-sitter-php
//!
//! This module provides robust PHP execution capabilities by parsing PHP code
//! into an Abstract Syntax Tree (AST) using tree-sitter-php and then interpreting it.

use anyhow::{anyhow, Result};
use async_recursion::async_recursion;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tracing::{debug, warn};
use tree_sitter::{Node, Parser};
use tree_sitter_php;

use bytes::Bytes;
use reqwest::Client;
use sha1::{Sha1, Digest};
use rustls::pki_types::ServerName;
use serde_json::Value as JsonValue;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex, OwnedMutexGuard};
use tokio_rustls::TlsConnector;

use crate::xml_parser;

static PHP_TAG_RE: OnceLock<Regex> = OnceLock::new();
static SESSION_LOCKS: OnceLock<std::sync::Mutex<HashMap<String, Arc<AsyncMutex<()>>>>> =
    OnceLock::new();

/// Active TLS socket stream for PHP stream functions
struct PhpTlsStream {
    stream: tokio_rustls::client::TlsStream<TcpStream>,
    eof: bool,
    /// Buffer for efficient line reading (fgets) and fread
    read_buffer: Vec<u8>,
    /// Write buffer for coalescing small writes
    write_buffer: Vec<u8>,
    /// Real-time streaming mode (disables write buffering for immediate flush)
    streaming_mode: bool,
}

/// AST-based PHP processor using tree-sitter-php
/// A stored user-defined PHP function
#[derive(Clone, Debug)]
struct PhpFunction {
    /// Parameter names (without $) and optional default values
    params: Vec<(String, Option<PhpValue>)>,
    /// The raw source code of the function body (between { and })
    body_source: String,
}

/// A user-defined PHP class
#[derive(Clone, Debug)]
struct PhpClass {
    #[allow(dead_code)]
    name: String,
    parent: Option<String>,
    /// Methods keyed by lowercase name
    methods: HashMap<String, PhpFunction>,
    /// Default property values declared in the class body
    default_props: HashMap<String, PhpValue>,
}

/// A PHP object instance (value semantics — cloned on mutation boundary like PHP arrays)
#[derive(Clone, Debug)]
pub struct PhpObject {
    pub class_name: String,
    pub props: HashMap<String, PhpValue>,
}

/// Sentinel for PHP throw expressions — caught by try/catch
#[derive(Debug)]
struct PhpThrow {
    class_name: String,
    message: String,
}
impl std::fmt::Display for PhpThrow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.class_name, self.message)
    }
}
impl std::error::Error for PhpThrow {}

/// State for curl_* emulation — accumulates options until curl_exec()
#[derive(Clone, Debug, Default)]
struct PhpCurlHandle {
    url: String,
    method: String,
    headers: Vec<(String, String)>,
    post_fields: String,
    return_transfer: bool,
    follow_location: bool,
    timeout: u64,
    /// Response status code (set after curl_exec)
    response_status: u16,
    /// Response content type (set after curl_exec)
    response_content_type: String,
    /// Last error message from curl_exec
    last_error: String,
}

pub struct AstPhpProcessor {
    parser: Parser,
    source_code: String,
    global_variables: HashMap<String, PhpValue>,
    variables: HashMap<String, PhpValue>,
    superglobals: HashMap<String, HashMap<String, String>>,
    response_status: u16,
    response_headers: HashMap<String, String>,
    response_body_override: Option<String>,
    current_template_path: Option<PathBuf>,
    root_dir: Option<PathBuf>,
    http_client: Client,
    included_files: HashSet<PathBuf>,
    output_buffers: Vec<String>,
    side_effect_output: Option<String>,
    /// User-defined functions: name -> PhpFunction
    user_functions: HashMap<String, PhpFunction>,
    /// Constants defined via define()
    constants: HashMap<String, PhpValue>,
    /// Accumulated output across all nested execute_php_ast calls (survives exit)
    accumulated_output: String,
    /// Return value from top-level return statement
    script_returned: Option<bool>,
    /// Actual PHP value returned by a script (for $x = require 'file.php')
    script_return_value: Option<PhpValue>,
    /// Active TLS socket streams keyed by handle ID
    streams: HashMap<u32, PhpTlsStream>,
    /// Next stream handle ID
    next_stream_id: u32,
    /// Per-request error log callback (routes to domain-specific log files)
    error_log_handler: Option<Arc<dyn Fn(&str) + Send + Sync>>,
    /// Set to true when a `continue` statement is hit inside a loop body
    loop_continue: bool,
    /// Streaming mode: channel sender for body chunks (set before execute_chain)
    flush_tx: Option<mpsc::Sender<Result<Bytes, std::io::Error>>>,
    /// Streaming mode: oneshot for sending response headers on first flush
    headers_tx: Option<oneshot::Sender<(HashMap<String, String>, u16)>>,
    /// Streaming mode: accumulates echo output between flush() calls
    streaming_buf: String,
    /// Active ruph session id for the current request, set by ruph_session_start($id)
    session_id: Option<String>,
    /// Whether the current ruph session was destroyed during the request
    session_destroyed: bool,
    /// Per-process lock held while a ruph session is active in this request
    session_lock: Option<OwnedMutexGuard<()>>,
    /// User-defined classes keyed by name (preserved across requests like user_functions)
    user_classes: HashMap<String, PhpClass>,
    /// Active curl handles keyed by resource ID
    curl_handles: HashMap<u32, PhpCurlHandle>,
    /// Next curl handle ID
    next_curl_id: u32,
    /// Domain for the current request (for log context)
    request_domain: String,
    /// Request URI for the current request (for log context)
    request_uri: String,
}

impl AstPhpProcessor {
    /// Create a new AST-based PHP processor
    pub fn new() -> Result<Self> {
        debug!("Initializing AST-based PHP processor using tree-sitter-php");

        let mut parser = Parser::new();
        let language = tree_sitter_php::LANGUAGE_PHP;
        parser
            .set_language(&language.into())
            .map_err(|_| anyhow!("Error loading PHP grammar"))?;

        Ok(Self {
            parser,
            source_code: String::new(),
            global_variables: HashMap::new(),
            variables: HashMap::new(),
            superglobals: HashMap::new(),
            response_status: 200,
            response_headers: HashMap::new(),
            response_body_override: None,
            current_template_path: None,
            root_dir: None,
            http_client: Client::builder()
                .pool_max_idle_per_host(2)
                .pool_idle_timeout(std::time::Duration::from_secs(30))
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| Client::new()),
            included_files: HashSet::new(),
            output_buffers: Vec::new(),
            side_effect_output: None,
            user_functions: HashMap::new(),
            accumulated_output: String::new(),
            script_returned: None,
            script_return_value: None,
            streams: HashMap::new(),
            next_stream_id: 1,
            error_log_handler: None,
            loop_continue: false,
            flush_tx: None,
            headers_tx: None,
            streaming_buf: String::new(),
            session_id: None,
            session_destroyed: false,
            session_lock: None,
            user_classes: {
                let mut cls = HashMap::new();
                // PHP built-in exception hierarchy
                let exception_props: HashMap<String, PhpValue> = [
                    ("message".to_string(), PhpValue::String(String::new())),
                    ("code".to_string(), PhpValue::Int(0)),
                    ("file".to_string(), PhpValue::String(String::new())),
                    ("line".to_string(), PhpValue::Int(0)),
                ].into_iter().collect();
                cls.insert("Exception".to_string(), PhpClass {
                    name: "Exception".to_string(),
                    parent: None,
                    methods: HashMap::new(),
                    default_props: exception_props.clone(),
                });
                cls.insert("Throwable".to_string(), PhpClass {
                    name: "Throwable".to_string(),
                    parent: None,
                    methods: HashMap::new(),
                    default_props: HashMap::new(),
                });
                for name in [
                    "RuntimeException", "InvalidArgumentException", "LogicException",
                    "BadMethodCallException", "BadFunctionCallException", "DomainException",
                    "LengthException", "OutOfRangeException", "OverflowException",
                    "RangeException", "UnderflowException", "UnexpectedValueException",
                    "ErrorException", "TypeError", "ValueError", "Error",
                ] {
                    cls.insert(name.to_string(), PhpClass {
                        name: name.to_string(),
                        parent: Some("Exception".to_string()),
                        methods: HashMap::new(),
                        default_props: exception_props.clone(),
                    });
                }
                cls
            },
            curl_handles: HashMap::new(),
            next_curl_id: 1,
            request_domain: String::new(),
            request_uri: String::new(),
            constants: {
                let mut c = HashMap::new();
                c.insert("PATHINFO_DIRNAME".to_string(), PhpValue::Int(1));
                c.insert("PATHINFO_BASENAME".to_string(), PhpValue::Int(2));
                c.insert("PATHINFO_EXTENSION".to_string(), PhpValue::Int(4));
                c.insert("PATHINFO_FILENAME".to_string(), PhpValue::Int(8));
                c.insert("PHP_URL_SCHEME".to_string(), PhpValue::Int(0));
                c.insert("PHP_URL_HOST".to_string(), PhpValue::Int(1));
                c.insert("PHP_URL_PORT".to_string(), PhpValue::Int(2));
                c.insert("PHP_URL_USER".to_string(), PhpValue::Int(3));
                c.insert("PHP_URL_PASS".to_string(), PhpValue::Int(4));
                c.insert("PHP_URL_PATH".to_string(), PhpValue::Int(5));
                c.insert("PHP_URL_QUERY".to_string(), PhpValue::Int(6));
                c.insert("PHP_URL_FRAGMENT".to_string(), PhpValue::Int(7));
                c.insert("FILE_APPEND".to_string(), PhpValue::Int(8));
                c.insert("LOCK_EX".to_string(), PhpValue::Int(2));
                c.insert("E_USER_ERROR".to_string(), PhpValue::Int(256));
                c.insert("E_USER_WARNING".to_string(), PhpValue::Int(512));
                c.insert("E_USER_NOTICE".to_string(), PhpValue::Int(1024));
                c.insert("E_USER_DEPRECATED".to_string(), PhpValue::Int(16384));
                c.insert("PHP_EOL".to_string(), PhpValue::String("\n".to_string()));
                c.insert("PHP_INT_MAX".to_string(), PhpValue::Int(i64::MAX));
                c.insert("PHP_INT_MIN".to_string(), PhpValue::Int(i64::MIN));
                c.insert(
                    "DIRECTORY_SEPARATOR".to_string(),
                    PhpValue::String("/".to_string()),
                );
                c.insert(
                    "PHP_SAPI".to_string(),
                    PhpValue::String("ruph-ast".to_string()),
                );
                c.insert(
                    "PHP_VERSION".to_string(),
                    PhpValue::String("8.4.0-ruph".to_string()),
                );
                c.insert("TRUE".to_string(), PhpValue::Bool(true));
                c.insert("FALSE".to_string(), PhpValue::Bool(false));
                c.insert("NULL".to_string(), PhpValue::Null);
                c.insert("STREAM_CLIENT_CONNECT".to_string(), PhpValue::Int(4));
                c.insert("STREAM_CLIENT_ASYNC_CONNECT".to_string(), PhpValue::Int(2));
                c.insert("STREAM_CLIENT_PERSISTENT".to_string(), PhpValue::Int(1));
                // CURL constants
                c.insert("CURLOPT_URL".to_string(), PhpValue::Int(10002));
                c.insert("CURLOPT_RETURNTRANSFER".to_string(), PhpValue::Int(19913));
                c.insert("CURLOPT_POST".to_string(), PhpValue::Int(47));
                c.insert("CURLOPT_POSTFIELDS".to_string(), PhpValue::Int(10015));
                c.insert("CURLOPT_HTTPHEADER".to_string(), PhpValue::Int(10023));
                c.insert("CURLOPT_FOLLOWLOCATION".to_string(), PhpValue::Int(52));
                c.insert("CURLOPT_TIMEOUT".to_string(), PhpValue::Int(13));
                c.insert("CURLOPT_CONNECTTIMEOUT".to_string(), PhpValue::Int(78));
                c.insert("CURLOPT_SSL_VERIFYPEER".to_string(), PhpValue::Int(64));
                c.insert("CURLOPT_SSL_VERIFYHOST".to_string(), PhpValue::Int(81));
                c.insert("CURLOPT_USERAGENT".to_string(), PhpValue::Int(10018));
                c.insert("CURLOPT_CUSTOMREQUEST".to_string(), PhpValue::Int(10036));
                c.insert("CURLOPT_HEADER".to_string(), PhpValue::Int(42));
                c.insert("CURLINFO_HTTP_CODE".to_string(), PhpValue::Int(2097154));
                c.insert("CURLINFO_RESPONSE_CODE".to_string(), PhpValue::Int(2097154));
                c.insert("CURLINFO_CONTENT_TYPE".to_string(), PhpValue::Int(1048594));
                c.insert("CURLOPT_ENCODING".to_string(), PhpValue::Int(10102));
                // JSON constants
                c.insert("JSON_UNESCAPED_SLASHES".to_string(), PhpValue::Int(64));
                c.insert("JSON_UNESCAPED_UNICODE".to_string(), PhpValue::Int(256));
                c.insert("JSON_PRETTY_PRINT".to_string(), PhpValue::Int(128));
                c.insert("JSON_FORCE_OBJECT".to_string(), PhpValue::Int(16));
                c.insert("JSON_THROW_ON_ERROR".to_string(), PhpValue::Int(4194304));
                // HTML entity constants
                c.insert("ENT_QUOTES".to_string(), PhpValue::Int(3));
                c.insert("ENT_HTML5".to_string(), PhpValue::Int(48));
                c.insert("ENT_XML1".to_string(), PhpValue::Int(16));
                c.insert("ENT_COMPAT".to_string(), PhpValue::Int(2));
                // PCRE constants
                c.insert("PREG_SET_ORDER".to_string(), PhpValue::Int(2));
                c.insert("PREG_PATTERN_ORDER".to_string(), PhpValue::Int(1));
                c.insert("PREG_OFFSET_CAPTURE".to_string(), PhpValue::Int(256));
                // http_build_query constants
                c.insert("PHP_QUERY_RFC1738".to_string(), PhpValue::Int(1));
                c.insert("PHP_QUERY_RFC3986".to_string(), PhpValue::Int(2));
                // libxml constants
                c.insert("LIBXML_NOCDATA".to_string(), PhpValue::Int(16384));
                c.insert("LIBXML_NOERROR".to_string(), PhpValue::Int(32));
                c
            },
        })
    }

    /// Log a message to the domain-specific error log (or fall back to tracing::warn).
    fn log_error(&self, msg: &str) {
        if let Some(ref handler) = self.error_log_handler {
            handler(msg);
        } else {
            warn!("[{}] PHP: {}", self.request_domain, msg);
        }
    }

    fn log_error_with_context(&self, msg: &str) {
        let ctx = format!("[{} {}] {}", self.request_domain, self.request_uri, msg);
        if let Some(ref handler) = self.error_log_handler {
            handler(&ctx);
        } else {
            warn!("PHP: {}", ctx);
        }
    }

    /// Execute PHP code with environment variables using AST parsing
    pub async fn execute_php(
        &mut self,
        php_code: &str,
        get_params: &HashMap<String, String>,
        post_params: &HashMap<String, String>,
        server_vars: &HashMap<String, String>,
        template_path: &Path,
        root_dir: &Path,
    ) -> Result<PhpExecution> {
        self.execute_php_with_handler(
            php_code,
            get_params,
            post_params,
            server_vars,
            template_path,
            root_dir,
            None,
        )
        .await
    }

    /// Execute PHP code with an optional error log handler for domain-specific logging.
    pub async fn execute_php_with_handler(
        &mut self,
        php_code: &str,
        get_params: &HashMap<String, String>,
        post_params: &HashMap<String, String>,
        server_vars: &HashMap<String, String>,
        template_path: &Path,
        root_dir: &Path,
        error_handler: Option<Arc<dyn Fn(&str) + Send + Sync>>,
    ) -> Result<PhpExecution> {
        debug!("Executing PHP code with AST processor");

        self.reset_request_state(template_path, root_dir);
        self.error_log_handler = error_handler;
        self.request_domain = server_vars
            .get("HTTP_HOST")
            .cloned()
            .unwrap_or_default();
        self.request_uri = server_vars
            .get("REQUEST_URI")
            .cloned()
            .unwrap_or_default();

        // Set up superglobals
        self.superglobals
            .insert("_GET".to_string(), get_params.clone());
        self.superglobals
            .insert("_POST".to_string(), post_params.clone());
        self.superglobals
            .insert("_SERVER".to_string(), server_vars.clone());
        self.superglobals.insert(
            "_COOKIE".to_string(),
            Self::parse_cookie_header(server_vars),
        );
        self.superglobals
            .insert("_SESSION".to_string(), HashMap::new());

        let mut request_params = get_params.clone();
        request_params.extend(post_params.clone());
        self.superglobals
            .insert("_REQUEST".to_string(), request_params);

        let mut output = String::new();
        self.accumulated_output.clear();

        self.execute_php_body(php_code, &mut output).await
    }

    /// Shared execution body used by both execute_php_with_handler and execute_php_continue.
    async fn execute_php_body(
        &mut self,
        php_code: &str,
        output: &mut String,
    ) -> Result<PhpExecution> {
        // Process the PHP code — catch exit/die as normal termination
        let mut exited = false;
        match self.process_php_code(php_code, output).await {
            Ok(_) => {}
            Err(e) => {
                if e.downcast_ref::<PhpExit>().is_some() {
                    // exit/die is normal PHP termination — not an error
                    debug!("PHP script called exit");
                    exited = true;
                    // Merge accumulated output from nested calls that was saved before exit
                    if output.is_empty() && !self.accumulated_output.is_empty() {
                        *output = std::mem::take(&mut self.accumulated_output);
                    }
                } else {
                    return Err(e);
                }
            }
        }

        let body = self
            .response_body_override
            .clone()
            .unwrap_or_else(|| output.clone());

        // Match PHP behavior: Location header without explicit status → 302
        let status =
            if self.response_status == 200 && self.response_headers.contains_key("location") {
                302
            } else {
                self.response_status
            };

        self.finalize_session().await;

        Ok(PhpExecution {
            body,
            status,
            headers: self.response_headers.clone(),
            exited,
            returned: self.script_returned,
        })
    }

    /// Process PHP code and return output
    async fn process_php_code(&mut self, code: &str, output: &mut String) -> Result<String> {
        let mut current_pos = 0;

        // Find PHP tags (both <?php ... ?> and <?= ... ?>)
        let php_tag_regex =
            PHP_TAG_RE.get_or_init(|| Regex::new(r"(?s)<\?(php|=)?(.*?)\?>").unwrap());

        let mut found_tag = false;

        for cap in php_tag_regex.captures_iter(code) {
            found_tag = true;
            let full_match = cap.get(0).unwrap();
            let tag_kind = cap.get(1).map(|m| m.as_str()).unwrap_or("");
            let mut php_code = cap.get(2).unwrap().as_str().to_string();

            if tag_kind == "=" {
                php_code = format!("echo {};", php_code.trim());
            }

            // Add HTML content before PHP tag
            if full_match.start() > current_pos {
                output.push_str(&code[current_pos..full_match.start()]);
            }

            // Process PHP code
            let php_output = self.execute_php_ast(&php_code).await?;
            output.push_str(&php_output);

            current_pos = full_match.end();
        }

        // If no PHP tags were found but code contains a PHP open tag without closing,
        // treat the whole file as PHP (common when the closing tag is omitted).
        if !found_tag && code.contains("<?php") {
            let php_output = self.execute_php_ast(code).await?;
            output.push_str(&php_output);
            return Ok(output.clone());
        }

        // Add remaining HTML content
        if current_pos < code.len() {
            output.push_str(&code[current_pos..]);
        }

        Ok(output.clone())
    }

    /// Execute PHP code using AST parsing
    async fn execute_php_ast(&mut self, php_code: &str) -> Result<String> {
        // Wrap the code in PHP tags if not already present
        let wrapped_code = if php_code.trim_start().starts_with("<?php") {
            php_code.to_string()
        } else {
            format!("<?php {} ?>", php_code)
        };

        self.source_code = wrapped_code.clone();

        self.parser
            .set_language(&tree_sitter_php::LANGUAGE_PHP.into())
            .map_err(|_| anyhow!("Error loading PHP grammar"))?;

        // Parse the PHP code into AST
        let tree = self
            .parser
            .parse(&wrapped_code, None)
            .ok_or_else(|| anyhow!("Failed to parse PHP code"))?;

        if tree.root_node().has_error() {
            self.log_error("AST parse error in PHP code");
            debug!("AST tree: {}", tree.root_node().to_sexp());
        }

        // Interpret the AST
        let mut output = String::new();
        let flow = match self.process_node(tree.root_node(), &mut output).await {
            Ok(flow) => flow,
            Err(e) => {
                // Save accumulated output before propagating exit/error
                self.accumulated_output.push_str(&output);
                return Err(e);
            }
        };
        if let ControlFlow::Break(()) = flow {
            debug!("AST output length: {} (break)", output.len());
            return Ok(output);
        }

        debug!("AST output length: {}", output.len());
        Ok(output)
    }

    #[allow(dead_code)]
    async fn execute_php_ast_php_only(&mut self, php_code: &str) -> Result<String> {
        let mut code = php_code.to_string();
        if let Some(pos) = code.find("<?php") {
            code = code[(pos + 5)..].to_string();
        }
        if let Some(pos) = code.find("?>") {
            code = code[..pos].to_string();
        }

        let wrapped_code = format!("<?php {} ?>", code);

        self.parser
            .set_language(&tree_sitter_php::LANGUAGE_PHP.into())
            .map_err(|_| anyhow!("Error loading PHP grammar"))?;

        self.source_code = wrapped_code.clone();
        let tree = self
            .parser
            .parse(&wrapped_code, None)
            .ok_or_else(|| anyhow!("Failed to parse PHP code"))?;

        if tree.root_node().has_error() {
            self.log_error("AST parse error in PHP code");
            debug!("AST tree: {}", tree.root_node().to_sexp());
        }

        if tree.root_node().has_error() {
            self.log_error("PHP-only parse has errors");
        }

        let mut output = String::new();
        let flow = self.process_node(tree.root_node(), &mut output).await?;
        if let ControlFlow::Break(()) = flow {
            if output.trim().is_empty() {
                debug!("PHP fallback tree: {}", tree.root_node().to_sexp());
            }
            debug!("PHP fallback output length: {}", output.len());
            // Restore full PHP grammar for subsequent parses
            self.parser
                .set_language(&tree_sitter_php::LANGUAGE_PHP.into())
                .map_err(|_| anyhow!("Error loading PHP grammar"))?;
            return Ok(output);
        }

        if output.trim().is_empty() {
            debug!("PHP fallback tree: {}", tree.root_node().to_sexp());
        }
        debug!("PHP fallback output length: {}", output.len());
        // Restore full PHP grammar for subsequent parses
        self.parser
            .set_language(&tree_sitter_php::LANGUAGE_PHP.into())
            .map_err(|_| anyhow!("Error loading PHP grammar"))?;

        Ok(output)
    }

    /// Process a node in the AST
    #[async_recursion]
    async fn process_node(
        &mut self,
        node: Node<'async_recursion>,
        output: &mut String,
    ) -> Result<ControlFlow<()>> {
        match node.kind() {
            "text" | "inline_html" => {
                let text = {
                    let source = self.source_code.as_bytes();
                    node.utf8_text(source)
                        .map_err(|_| anyhow!("Failed to get text"))?
                };
                self.append_output(output, &text.to_string());
            }
            "echo_statement" => {
                let mut handled = false;
                for child in node.named_children(&mut node.walk()) {
                    if child.kind() == "argument_list" {
                        self.process_argument_list(child, output).await?;
                        handled = true;
                    } else {
                        let value = self.evaluate_expression(child).await?;
                        if let Some(out) = self.take_side_effect_output() {
                            self.append_output(output, &out);
                        }
                        self.append_output(output, &value.as_string());
                        handled = true;
                    }
                }
                if !handled {
                    if let Some(child) = node.named_child(0) {
                        let value = self.evaluate_expression(child).await?;
                        if let Some(out) = self.take_side_effect_output() {
                            self.append_output(output, &out);
                        }
                        self.append_output(output, &value.as_string());
                    }
                }
            }
            "unset_statement" => {
                // Handle unset($var1, $var2, ...)
                if let Some(args_node) = node.child_by_field_name("arguments") {
                    for arg in args_node.named_children(&mut args_node.walk()) {
                        match arg.kind() {
                            "variable_name" => {
                                let var_name = arg
                                    .utf8_text(self.source_code.as_bytes())
                                    .unwrap_or("")
                                    .trim_start_matches('$');
                                // Remove from local variables first, then global
                                self.variables.remove(var_name);
                                self.global_variables.remove(var_name);
                            }
                            "subscript_expression" => {
                                // Handle array element unset: unset($arr['key'])
                                // For now, just evaluate the expression (which might have side effects)
                                // Full array element removal would require more complex handling
                                let _ = self.evaluate_expression(arg).await?;
                            }
                            "member_access_expression" => {
                                // Handle object property unset: unset($obj->prop)
                                // For now, just evaluate the expression
                                let _ = self.evaluate_expression(arg).await?;
                            }
                            _ => {
                                // Try to evaluate as a general expression
                                let _ = self.evaluate_expression(arg).await?;
                            }
                        }
                    }
                }
            }
            "return_statement" => {
                let return_value = node
                    .child_by_field_name("argument")
                    .or_else(|| node.child_by_field_name("value"))
                    .or_else(|| node.named_child(0));
                if let Some(value_node) = return_value {
                    let value = self.evaluate_expression(value_node).await?;
                    self.script_returned = Some(value.is_truthy());
                    self.response_body_override = Some(value.as_string());
                    self.script_return_value = Some(value);
                } else {
                    // bare `return;` — treated as return false (handled)
                    self.script_returned = Some(false);
                    self.script_return_value = None;
                }
                return Ok(ControlFlow::Break(()));
            }
            // ── exit / die ──────────────────────────────────────────────
            "exit_statement" => {
                // exit; or exit(code); or die; or die(msg);
                let code = if let Some(child) = node.named_child(0) {
                    let val = self.evaluate_expression(child).await?;
                    match val {
                        PhpValue::Int(n) => n as i32,
                        PhpValue::String(s) => {
                            // exit("message") outputs the message first
                            self.append_output(output, &s);
                            0
                        }
                        _ => 0,
                    }
                } else {
                    0
                };
                return Err(anyhow!(PhpExit { code }));
            }
            "assignment_expression" => {
                let left = node.child_by_field_name("left");
                let right = node.child_by_field_name("right");
                if let (Some(left), Some(right)) = (left, right) {
                    let value = self.evaluate_expression(right).await?;
                    self.assign_to(left, value).await?;
                }
            }
            // ── augmented assignment (+=, -=, .=, etc.) ─────────────────
            "augmented_assignment_expression" => {
                let left = node.child_by_field_name("left");
                let right = node.child_by_field_name("right");
                let operator = {
                    let source = self.source_code.as_bytes();
                    let mut op = String::new();
                    for i in 0..node.child_count() {
                        if let Some(child) = node.child(i) {
                            if !child.is_named() {
                                let text = child.utf8_text(source).unwrap_or("");
                                if text.ends_with('=') && text.len() >= 2 {
                                    op = text.to_string();
                                    break;
                                }
                            }
                        }
                    }
                    op
                };
                if let (Some(left), Some(right)) = (left, right) {
                    let left_val = self.evaluate_expression(left).await?;
                    let right_val = self.evaluate_expression(right).await?;
                    let result = match operator.as_str() {
                        "+=" => {
                            if left_val.is_int_like() && right_val.is_int_like() {
                                PhpValue::Int(left_val.as_int().wrapping_add(right_val.as_int()))
                            } else {
                                PhpValue::Float(left_val.as_float() + right_val.as_float())
                            }
                        }
                        "-=" => {
                            if left_val.is_int_like() && right_val.is_int_like() {
                                PhpValue::Int(left_val.as_int().wrapping_sub(right_val.as_int()))
                            } else {
                                PhpValue::Float(left_val.as_float() - right_val.as_float())
                            }
                        }
                        "*=" => {
                            if left_val.is_int_like() && right_val.is_int_like() {
                                PhpValue::Int(left_val.as_int().wrapping_mul(right_val.as_int()))
                            } else {
                                PhpValue::Float(left_val.as_float() * right_val.as_float())
                            }
                        }
                        "/=" => {
                            let d = right_val.as_float();
                            if d == 0.0 {
                                self.log_error("Division by zero");
                                PhpValue::Bool(false)
                            } else if left_val.is_int_like() && right_val.is_int_like() {
                                let li = left_val.as_int();
                                let ri = right_val.as_int();
                                if ri != 0 && li % ri == 0 {
                                    PhpValue::Int(li / ri)
                                } else {
                                    PhpValue::Float(left_val.as_float() / d)
                                }
                            } else {
                                PhpValue::Float(left_val.as_float() / d)
                            }
                        }
                        ".=" => PhpValue::String(format!(
                            "{}{}",
                            left_val.as_string(),
                            right_val.as_string()
                        )),
                        "??=" => {
                            if left_val.is_null() {
                                right_val
                            } else {
                                left_val
                            }
                        }
                        _ => right_val,
                    };
                    self.assign_to(left, result).await?;
                }
            }
            "variable" => {
                let value = self.evaluate_expression(node).await?;
                self.append_output(output, &value.as_string());
            }
            "function_call_expression"
            | "member_call_expression"
            | "nullsafe_member_call_expression" => {
                let value = self.evaluate_expression(node).await?;
                if let Some(out) = self.take_side_effect_output() {
                    self.append_output(output, &out);
                } else {
                    // Some function calls may produce output directly (like user functions with echo)
                    let s = value.as_string();
                    if !s.is_empty() && !matches!(value, PhpValue::Null) {
                        // Don't output function return values implicitly from statements
                    }
                }
            }
            "expression_statement" => {
                if let Some(child) = node.named_child(0) {
                    let value = self.evaluate_expression(child).await?;
                    if let Some(out) = self.take_side_effect_output() {
                        self.append_output(output, &out);
                    } else {
                        let rendered = value.as_string();
                        if !rendered.is_empty() && !matches!(value, PhpValue::Null) {
                            // Only expression_statement auto-outputs for non-null non-function values
                            // (matches PHP behavior where bare expressions don't output)
                        }
                    }
                }
            }
            // ── if / else / elseif ──────────────────────────────────────
            "if_statement" => {
                return self.process_if_statement(node, output).await;
            }
            // ── while loop ──────────────────────────────────────────────
            "while_statement" => {
                let condition = node
                    .child_by_field_name("condition")
                    .or_else(|| node.child_by_field_name("test"));
                let body = node.child_by_field_name("body");

                if let (Some(condition), Some(body)) = (condition, body) {
                    let mut iterations = 0;
                    loop {
                        let cond_val = self.evaluate_expression(condition).await?;
                        if !cond_val.is_truthy() {
                            break;
                        }
                        let flow = self.process_node(body, output).await?;
                        if let ControlFlow::Break(()) = flow {
                            if self.loop_continue {
                                self.loop_continue = false;
                                // continue: recheck condition and iterate
                            } else if self.script_returned.is_some() {
                                // return inside loop: propagate upward
                                return Ok(flow);
                            } else {
                                // break: exit loop
                                break;
                            }
                        }
                        iterations += 1;
                        if iterations > 100_000 {
                            warn!("while loop exceeded 100k iterations, breaking");
                            break;
                        }
                    }
                }
            }
            // ── for loop ────────────────────────────────────────────────
            "for_statement" => {
                // for (init; condition; update) { body }
                // tree-sitter-php field names: initializer, condition, update, body
                let init = node.child_by_field_name("initializer");
                let condition = node.child_by_field_name("condition");
                let update = node.child_by_field_name("update");
                let body = node.child_by_field_name("body");

                if let Some(init) = init {
                    self.evaluate_expression(init).await?;
                }
                let mut iterations = 0;
                loop {
                    if let Some(condition) = condition {
                        let cond_val = self.evaluate_expression(condition).await?;
                        if !cond_val.is_truthy() {
                            break;
                        }
                    }
                    if let Some(body) = body {
                        let flow = self.process_node(body, output).await?;
                        if let ControlFlow::Break(()) = flow {
                            if self.loop_continue {
                                self.loop_continue = false;
                                // continue: run update then recheck condition
                            } else if self.script_returned.is_some() {
                                return Ok(flow);
                            } else {
                                break;
                            }
                        }
                    }
                    if let Some(update) = update {
                        self.evaluate_expression(update).await?;
                    }
                    iterations += 1;
                    if iterations > 100_000 {
                        warn!("for loop exceeded 100k iterations, breaking");
                        break;
                    }
                }
            }
            // ── foreach ─────────────────────────────────────────────────
            "foreach_statement" => {
                // tree-sitter-php foreach_statement:
                //   children: expression (collection), then either a variable or a pair ($k => $v)
                //   field "body": the loop body statement
                let mut collection_node = None;
                let mut value_var_node = None;
                let mut key_var_node = None;

                let mut body_node = None;
                for child in node.named_children(&mut node.walk()) {
                    match child.kind() {
                        // first variable/expression child is the collection
                        "variable"
                        | "variable_name"
                        | "subscript_expression"
                        | "member_access_expression"
                        | "member_call_expression"
                        | "nullsafe_member_call_expression"
                        | "function_call_expression"
                        | "array_creation_expression"
                        | "parenthesized_expression"
                            if collection_node.is_none() =>
                        {
                            collection_node = Some(child);
                        }
                        // $k => $v produces a "pair" node
                        "pair" => {
                            if let Some(k) = child.named_child(0) {
                                key_var_node = Some(k);
                            }
                            if let Some(v) = child.named_child(1) {
                                value_var_node = Some(v);
                            }
                        }
                        // plain foreach($arr as $v) — second variable is the value
                        "variable" | "variable_name"
                            if collection_node.is_some() && value_var_node.is_none() =>
                        {
                            value_var_node = Some(child);
                        }
                        // anything else after collection+vars is the body
                        _ if collection_node.is_some()
                            && (value_var_node.is_some() || key_var_node.is_some())
                            && body_node.is_none() =>
                        {
                            body_node = Some(child);
                        }
                        _ => {}
                    }
                }

                let body = node.child_by_field_name("body").or(body_node);

                if let (Some(collection), Some(body)) = (collection_node, body) {
                    let collection_value = self.evaluate_expression(collection).await?;
                    if let PhpValue::Array(items) = collection_value {
                        for (index, item) in items.iter().enumerate() {
                            if let Some(value_node) = value_var_node {
                                if let Some(value_name) = self.get_identifier(value_node) {
                                    let value = match item {
                                        PhpArrayItem::KeyValue(_, value) => value.clone(),
                                        PhpArrayItem::Value(value) => value.clone(),
                                    };
                                    self.variables.insert(value_name, value);
                                }
                            }
                            if let Some(key_node) = key_var_node {
                                if let Some(key_name) = self.get_identifier(key_node) {
                                    let key_value = match item {
                                        PhpArrayItem::KeyValue(key, _) => {
                                            PhpValue::String(key.clone())
                                        }
                                        PhpArrayItem::Value(_) => {
                                            PhpValue::String(index.to_string())
                                        }
                                    };
                                    self.variables.insert(key_name, key_value);
                                }
                            }

                            let flow = self.process_node(body, output).await?;
                            if let ControlFlow::Break(()) = flow {
                                if self.loop_continue {
                                    self.loop_continue = false;
                                    // continue: move to next element
                                } else if self.script_returned.is_some() {
                                    return Ok(flow);
                                } else {
                                    // break: exit foreach
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            // ── switch ──────────────────────────────────────────────────
            "switch_statement" => {
                let test = node
                    .child_by_field_name("value")
                    .or_else(|| node.child_by_field_name("condition"));
                if let Some(test) = test {
                    let test_val = self.evaluate_expression(test).await?;
                    let body = node.child_by_field_name("body");
                    if let Some(body) = body {
                        let mut matched = false;
                        let mut fell_through = false;
                        for child in body.named_children(&mut body.walk()) {
                            match child.kind() {
                                "switch_case" => {
                                    if !fell_through {
                                        if let Some(value_node) = child.child_by_field_name("value")
                                        {
                                            let case_val =
                                                self.evaluate_expression(value_node).await?;
                                            if test_val.loose_eq(&case_val) {
                                                matched = true;
                                            }
                                        }
                                    }
                                    if matched || fell_through {
                                        fell_through = true;
                                        for stmt in child.named_children(&mut child.walk()) {
                                            if stmt.kind() == "break_statement" {
                                                fell_through = false;
                                                matched = false;
                                                break;
                                            }
                                            let flow = self.process_node(stmt, output).await?;
                                            if let ControlFlow::Break(()) = flow {
                                                return Ok(flow);
                                            }
                                        }
                                    }
                                }
                                "switch_default" => {
                                    if !matched {
                                        for stmt in child.named_children(&mut child.walk()) {
                                            if stmt.kind() == "break_statement" {
                                                break;
                                            }
                                            let flow = self.process_node(stmt, output).await?;
                                            if let ControlFlow::Break(()) = flow {
                                                return Ok(flow);
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
            // ── function definition ─────────────────────────────────────
            "function_definition" => {
                let name_node = node.child_by_field_name("name");
                let params_node = node.child_by_field_name("parameters");
                let body_node = node.child_by_field_name("body");

                if let (Some(name_node), Some(body_node)) = (name_node, body_node) {
                    let func_name = name_node
                        .utf8_text(self.source_code.as_bytes())?
                        .to_string();
                    let body_source = body_node
                        .utf8_text(self.source_code.as_bytes())?
                        .to_string();

                    let mut params = Vec::new();
                    if let Some(params_node) = params_node {
                        for child in params_node.named_children(&mut params_node.walk()) {
                            if child.kind() == "simple_parameter" || child.kind() == "parameter" {
                                if let Some(name) = child.child_by_field_name("name") {
                                    let pname = name
                                        .utf8_text(self.source_code.as_bytes())?
                                        .trim_start_matches('$')
                                        .to_string();
                                    let default = if let Some(def) =
                                        child.child_by_field_name("default_value")
                                    {
                                        Some(self.evaluate_expression(def).await?)
                                    } else {
                                        None
                                    };
                                    params.push((pname, default));
                                }
                            }
                        }
                    }

                    debug!("[{}] PHP func registered: {}() with {} params", self.request_domain, func_name, params.len());
                    self.user_functions.insert(
                        func_name,
                        PhpFunction {
                            params,
                            body_source,
                        },
                    );
                }
            }
            // ── class declaration ───────────────────────────────────────
            "class_declaration" => {
                let name_node = node.child_by_field_name("name");
                let Some(name_node) = name_node else { return Ok(ControlFlow::Continue(())) };
                let class_name = name_node
                    .utf8_text(self.source_code.as_bytes())?
                    .to_string();

                // Optional parent class
                let parent = node
                    .child_by_field_name("base_clause")
                    .or_else(|| {
                        // some grammars put the parent inside a named child
                        node.named_children(&mut node.walk())
                            .find(|c| c.kind() == "base_clause")
                    })
                    .and_then(|bc| bc.named_child(0))
                    .and_then(|n| n.utf8_text(self.source_code.as_bytes()).ok())
                    .map(|s| s.trim_start_matches('\\').to_string());

                let mut methods: HashMap<String, PhpFunction> = HashMap::new();
                let mut default_props: HashMap<String, PhpValue> = HashMap::new();

                // Find declaration_list body
                let body = node
                    .child_by_field_name("body")
                    .or_else(|| {
                        node.named_children(&mut node.walk())
                            .find(|c| c.kind() == "declaration_list")
                    });

                if let Some(body) = body {
                    for member in body.named_children(&mut body.walk()) {
                        match member.kind() {
                            "method_declaration" => {
                                let mname_node = member.child_by_field_name("name");
                                let mparams_node = member.child_by_field_name("parameters");
                                let mbody_node = member.child_by_field_name("body");
                                if let (Some(mname_node), Some(mbody_node)) =
                                    (mname_node, mbody_node)
                                {
                                    let method_name = mname_node
                                        .utf8_text(self.source_code.as_bytes())?
                                        .to_lowercase();
                                    let method_body = mbody_node
                                        .utf8_text(self.source_code.as_bytes())?
                                        .to_string();
                                    let mut params = Vec::new();
                                    if let Some(mparams_node) = mparams_node {
                                        for child in
                                            mparams_node.named_children(&mut mparams_node.walk())
                                        {
                                            if child.kind() == "simple_parameter"
                                                || child.kind() == "parameter"
                                            {
                                                if let Some(pname) =
                                                    child.child_by_field_name("name")
                                                {
                                                    let pname_str = pname
                                                        .utf8_text(self.source_code.as_bytes())?
                                                        .trim_start_matches('$')
                                                        .to_string();
                                                    let default = if let Some(def) =
                                                        child.child_by_field_name("default_value")
                                                    {
                                                        Some(
                                                            self.evaluate_expression(def).await?,
                                                        )
                                                    } else {
                                                        None
                                                    };
                                                    params.push((pname_str, default));
                                                }
                                            }
                                        }
                                    }
                                    methods.insert(method_name, PhpFunction { params, body_source: method_body });
                                }
                            }
                            "property_declaration" => {
                                for prop_el in member.named_children(&mut member.walk()) {
                                    if prop_el.kind() == "property_element" {
                                        if let Some(prop_name_node) =
                                            prop_el.child_by_field_name("name")
                                        {
                                            let prop_name = prop_name_node
                                                .utf8_text(self.source_code.as_bytes())?
                                                .trim_start_matches('$')
                                                .to_string();
                                            let default_val =
                                                if let Some(def) =
                                                    prop_el.child_by_field_name("default_value")
                                                {
                                                    self.evaluate_expression(def).await?
                                                } else {
                                                    PhpValue::Null
                                                };
                                            default_props.insert(prop_name, default_val);
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }

                debug!("PHP class: {} (parent={:?}, methods={}, props={})",
                    class_name,
                    parent,
                    methods.len(),
                    default_props.len()
                );
                self.user_classes.insert(
                    class_name.clone(),
                    PhpClass { name: class_name, parent, methods, default_props },
                );
            }
            // ── interface declaration (register as empty class for instanceof) ──
            "interface_declaration" => {
                let name_node = node.child_by_field_name("name");
                if let Some(name_node) = name_node {
                    let iface_name = name_node
                        .utf8_text(self.source_code.as_bytes())?
                        .to_string();
                    // Register as a class with no methods/props so instanceof works
                    if !self.user_classes.contains_key(&iface_name) {
                        self.user_classes.insert(
                            iface_name.clone(),
                            PhpClass {
                                name: iface_name,
                                parent: None,
                                methods: HashMap::new(),
                                default_props: HashMap::new(),
                            },
                        );
                    }
                }
            }
            // ── trait declaration (register as class for method reuse) ──
            "trait_declaration" => {
                // Silently skip — traits are not executed
            }
            // ── try / catch / finally ──────────────────────────────────
            "try_statement" => {
                return self.process_try_catch(node, output).await;
            }
            // ── throw statement (standalone `throw new ...;`) ──────────
            "throw_statement" => {
                if let Some(expr) = node.named_child(0) {
                    let val = self.evaluate_expression(expr).await?;
                    let (class_name, message) = match val {
                        PhpValue::Object(obj) => {
                            let msg = obj.props.get("message").map_or(String::new(), |v| v.as_string());
                            debug!("[{}] PHP throw {}: props={:?}, msg='{}'", self.request_domain, obj.class_name, obj.props.keys().collect::<Vec<_>>(), msg);
                            (obj.class_name.clone(), msg)
                        }
                        _ => {
                            debug!("[{}] PHP throw (non-object): {:?}", self.request_domain, val);
                            ("Exception".to_string(), val.as_string())
                        }
                    };
                    return Err(anyhow!(PhpThrow { class_name, message }));
                }
            }
            // ── include / require ───────────────────────────────────────
            "include_expression"
            | "require_expression"
            | "include_once_expression"
            | "require_once_expression" => {
                let is_once = node.kind().contains("_once");
                let target = node
                    .child_by_field_name("path")
                    .or_else(|| node.child_by_field_name("argument"))
                    .or_else(|| node.named_child(0));
                if let Some(target) = target {
                    let value = self.evaluate_expression(target).await?;
                    let (include_output, _) =
                        self.include_file(&value.as_string(), is_once).await?;
                    self.append_output(output, &include_output);
                }
            }
            // ── compound statement (block) ──────────────────────────────
            "compound_statement" => {
                for child in node.named_children(&mut node.walk()) {
                    let flow = self.process_node(child, output).await?;
                    if let ControlFlow::Break(()) = flow {
                        return Ok(flow);
                    }
                }
            }
            // ── break / continue (within loops) ─────────────────────────
            "break_statement" => {
                self.loop_continue = false;
                return Ok(ControlFlow::Break(()));
            }
            "continue_statement" => {
                self.loop_continue = true;
                return Ok(ControlFlow::Break(()));
            }
            // ── const declarations ───────────────────────────────────────
            "const_declaration" => {
                // const FOO = 'bar'; or const FOO = ['a','b'];
                for child in node.named_children(&mut node.walk()) {
                    if child.kind() == "const_element" {
                        let name_node = child
                            .child_by_field_name("name")
                            .or_else(|| child.named_child(0));
                        let value_node = child
                            .child_by_field_name("value")
                            .or_else(|| child.named_child(1));
                        if let (Some(name_n), Some(val_n)) = (name_node, value_node) {
                            let name = name_n.utf8_text(self.source_code.as_bytes())?.to_string();
                            let value = self.evaluate_expression(val_n).await?;
                            self.constants.insert(name, value);
                        }
                    }
                }
            }
            _ => {
                let kind = node.kind();
                // Expression-like nodes that ended up in process_node:
                // evaluate as expression (discarding result) instead of recursing into children
                if kind.ends_with("_expression")
                    || kind == "array_element_initializer"
                    || kind == "string"
                    || kind == "string_content"
                    || kind == "integer"
                    || kind == "float"
                    || kind == "encapsed_string"
                {
                    let _ = self.evaluate_expression(node).await.ok();
                    if let Some(out) = self.take_side_effect_output() {
                        self.append_output(output, &out);
                    }
                } else {
                    // Log unrecognized statement-level AST nodes
                    if !matches!(
                        kind,
                        "program"
                            | "php_tag"
                            | "php_end_tag"
                            | "comment"
                            | "text_interpolation"
                            | "expression_statement"
                            | "compound_statement"
                            | "declaration_list"
                            | "namespace_definition"
                            | "namespace_use_declaration"
                            | "const_element"
                            | "catch_clause"
                            | "finally_clause"
                            | "type_list"
                            | "named_type"
                            | "name"
                            | "variable_name"
                            | "visibility_modifier"
                            | "static_modifier"
                            | "abstract_modifier"
                            | "final_modifier"
                            | "readonly_modifier"
                            | "formal_parameters"
                            | "simple_parameter"
                            | "parameter"
                            | "primitive_type"
                            | "property_declaration"
                            | "property_element"
                            | "method_declaration"
                            | "class_interface_clause"
                            | "base_clause"
                            | "enum_declaration"
                            | "use_declaration"
                    ) {
                        let line = node.start_position().row + 1;
                        warn!("[{}] {} Unhandled AST node '{}' at line {}", self.request_domain, self.request_uri, kind, line);
                        self.log_error_with_context(&format!("Unhandled AST node '{}' at line {}", kind, line));
                    }
                    // Recursively process child nodes
                    for child in node.named_children(&mut node.walk()) {
                        let flow = self.process_node(child, output).await?;
                        if let ControlFlow::Break(()) = flow {
                            return Ok(ControlFlow::Break(()));
                        }
                    }
                }
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Process an argument list (used in echo statements)
    #[async_recursion]
    async fn process_argument_list(
        &mut self,
        node: Node<'async_recursion>,
        output: &mut String,
    ) -> Result<()> {
        for child in node.named_children(&mut node.walk()) {
            if child.kind() == "string" {
                // Handle string literals
                let text = child
                    .utf8_text(self.source_code.as_bytes())
                    .map_err(|_| anyhow!("Failed to get text"))?;
                // Remove surrounding quotes
                let trimmed = text.trim_matches(|c| c == '\'' || c == '"');
                self.append_output(output, &trimmed.to_string());
            } else if child.kind() == "parenthesized_expression" {
                // Process nested expressions
                let _ = self.process_node(child, output).await?;
            } else {
                // Handle other expressions (variables, functions)
                let value = self.evaluate_expression(child).await?;
                if let Some(out) = self.take_side_effect_output() {
                    self.append_output(output, &out);
                }
                self.append_output(output, &value.as_string());
            }
        }
        Ok(())
    }

    /// Process if/elseif/else statement
    #[async_recursion]
    async fn process_if_statement(
        &mut self,
        node: Node<'async_recursion>,
        output: &mut String,
    ) -> Result<ControlFlow<()>> {
        // Evaluate the condition
        let condition = node.child_by_field_name("condition");
        let body = node.child_by_field_name("body");
        let alternative = node.child_by_field_name("alternative");

        let cond_result = if let Some(condition) = condition {
            let val = self.evaluate_expression(condition).await?;
            val.is_truthy()
        } else {
            // Fallback: walk children to find parenthesized_expression before body
            let mut result = false;
            for child in node.named_children(&mut node.walk()) {
                if child.kind() == "parenthesized_expression" {
                    let val = self.evaluate_expression(child).await?;
                    result = val.is_truthy();
                    break;
                }
            }
            result
        };

        if cond_result {
            if let Some(body) = body {
                return self.process_node(body, output).await;
            }
            // Alternative: process first compound_statement child
            for child in node.named_children(&mut node.walk()) {
                if child.kind() == "compound_statement" {
                    return self.process_node(child, output).await;
                }
            }
        } else if let Some(alt) = alternative {
            // else or elseif
            match alt.kind() {
                "else_clause" => {
                    if let Some(body) = alt.child_by_field_name("body") {
                        return self.process_node(body, output).await;
                    }
                    // Fallback: process all named children of else clause
                    for child in alt.named_children(&mut alt.walk()) {
                        let flow = self.process_node(child, output).await?;
                        if let ControlFlow::Break(()) = flow {
                            return Ok(flow);
                        }
                    }
                }
                "else_if_clause" | "if_statement" => {
                    return self.process_if_statement(alt, output).await;
                }
                _ => {
                    return self.process_node(alt, output).await;
                }
            }
        } else {
            // No alternative matched — check for else_clause / else_if_clause as children
            let mut found_else = false;
            for child in node.named_children(&mut node.walk()) {
                if found_else {
                    // Already past else_clause header, process subsequent nodes
                }
                match child.kind() {
                    "else_clause" => {
                        for inner in child.named_children(&mut child.walk()) {
                            let flow = self.process_node(inner, output).await?;
                            if let ControlFlow::Break(()) = flow {
                                return Ok(flow);
                            }
                        }
                        found_else = true;
                    }
                    "else_if_clause" => {
                        return self.process_if_statement(child, output).await;
                    }
                    _ => {}
                }
            }
        }

        Ok(ControlFlow::Continue(()))
    }

    /// Assign a value to a left-hand side (variable, subscript, etc.)
    async fn assign_to(&mut self, left: Node<'_>, value: PhpValue) -> Result<()> {
        match left.kind() {
            "variable" | "variable_name" => {
                if let Some(var_name) = self.get_identifier(left) {
                    self.variables.insert(var_name, value);
                }
            }
            "subscript_expression" => {
                // $arr['key'] = value or $arr[] = value
                let target = left
                    .child_by_field_name("value")
                    .or_else(|| left.child_by_field_name("array"))
                    .or_else(|| left.named_child(0));
                let index = left
                    .child_by_field_name("index")
                    .or_else(|| left.child_by_field_name("offset"))
                    .or_else(|| left.named_child(1));

                if let Some(target) = target {
                    let target_name = if let Some(id) = self.get_identifier(target) {
                        id
                    } else {
                        target
                            .utf8_text(self.source_code.as_bytes())?
                            .trim_start_matches('$')
                            .to_string()
                    };

                    if let Some(index) = index {
                        let key = self.evaluate_expression(index).await?.as_string();

                        if let Some(superglobal) = self.superglobals.get_mut(&target_name) {
                            superglobal.insert(key, value.as_string());
                            return Ok(());
                        }

                        // Update existing array or create new one
                        let mut arr = if let Some(PhpValue::Array(items)) =
                            self.variables.get(&target_name)
                        {
                            items.clone()
                        } else {
                            Vec::new()
                        };

                        // Replace existing key or append
                        let mut replaced = false;
                        for item in arr.iter_mut() {
                            if let PhpArrayItem::KeyValue(k, v) = item {
                                if k == &key {
                                    *v = value.clone();
                                    replaced = true;
                                    break;
                                }
                            }
                        }
                        if !replaced {
                            arr.push(PhpArrayItem::KeyValue(key, value));
                        }
                        self.variables.insert(target_name, PhpValue::Array(arr));
                    } else {
                        if let Some(superglobal) = self.superglobals.get_mut(&target_name) {
                            let key = superglobal.len().to_string();
                            superglobal.insert(key, value.as_string());
                            return Ok(());
                        }

                        // $arr[] = value (append)
                        let mut arr = if let Some(PhpValue::Array(items)) =
                            self.variables.get(&target_name)
                        {
                            items.clone()
                        } else {
                            Vec::new()
                        };
                        arr.push(PhpArrayItem::Value(value));
                        self.variables.insert(target_name, PhpValue::Array(arr));
                    }
                }
            }
            // ── object property assignment: $obj->prop = value ─────────
            "member_access_expression" | "nullsafe_member_access_expression" => {
                let obj_node = left
                    .child_by_field_name("object")
                    .or_else(|| left.named_child(0));
                let name_node = left
                    .child_by_field_name("name")
                    .or_else(|| left.child_by_field_name("member"))
                    .or_else(|| left.named_child(1));
                if let (Some(obj_node), Some(name_node)) = (obj_node, name_node) {
                    let prop = name_node
                        .utf8_text(self.source_code.as_bytes())
                        .unwrap_or("")
                        .trim_start_matches('$')
                        .to_string();
                    let var_name = self
                        .get_identifier(obj_node)
                        .unwrap_or_else(|| {
                            obj_node
                                .utf8_text(self.source_code.as_bytes())
                                .unwrap_or("")
                                .trim_start_matches('$')
                                .to_string()
                        });
                    if let Some(PhpValue::Object(obj)) = self.variables.get_mut(&var_name) {
                        obj.props.insert(prop, value);
                    }
                }
            }
            _ => {
                if let Some(var_name) = self.get_identifier(left) {
                    self.variables.insert(var_name, value);
                }
            }
        }
        Ok(())
    }

    /// Remove a variable or array key (unset).
    #[async_recursion]
    async fn unset_target(&mut self, node: Node<'async_recursion>) -> Result<()> {
        match node.kind() {
            "variable" | "variable_name" => {
                if let Some(var_name) = self.get_identifier(node) {
                    self.variables.remove(&var_name);
                }
            }
            "subscript_expression" => {
                let target = node
                    .child_by_field_name("value")
                    .or_else(|| node.child_by_field_name("array"))
                    .or_else(|| node.named_child(0));
                let index = node
                    .child_by_field_name("index")
                    .or_else(|| node.child_by_field_name("offset"))
                    .or_else(|| node.named_child(1));

                if let (Some(target), Some(index)) = (target, index) {
                    let target_name = if let Some(id) = self.get_identifier(target) {
                        id
                    } else {
                        target
                            .utf8_text(self.source_code.as_bytes())?
                            .trim_start_matches('$')
                            .to_string()
                    };
                    let key = self.evaluate_expression(index).await?.as_string();

                    if let Some(superglobal) = self.superglobals.get_mut(&target_name) {
                        superglobal.remove(&key);
                        return Ok(());
                    }

                    if let Some(PhpValue::Array(items)) = self.variables.get(&target_name) {
                        let mut new_items = Vec::new();
                        for item in items {
                            match item {
                                PhpArrayItem::KeyValue(k, _) if k == &key => {} // skip = remove
                                _ => new_items.push(item.clone()),
                            }
                        }
                        self.variables
                            .insert(target_name, PhpValue::Array(new_items));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Evaluate an expression node to a string value
    fn unescape_sequence(&self, raw: &str) -> String {
        match raw {
            "\\n" => "\n".to_string(),
            "\\r" => "\r".to_string(),
            "\\t" => "\t".to_string(),
            "\\\\" => "\\".to_string(),
            "\\\"" => "\"".to_string(),
            _ => raw.to_string(),
        }
    }

    fn get_identifier(&self, node: Node<'_>) -> Option<String> {
        match node.kind() {
            "variable" | "variable_name" => {
                let source = self.source_code.as_bytes();
                let raw = node.utf8_text(source).ok()?;
                Some(raw.trim_start_matches('$').to_string())
            }
            "name" => {
                let source = self.source_code.as_bytes();
                let raw = node.utf8_text(source).ok()?;
                Some(raw.to_string())
            }
            _ => None,
        }
    }

    #[async_recursion]
    async fn evaluate_expression(&mut self, node: Node<'async_recursion>) -> Result<PhpValue> {
        match node.kind() {
            "encapsed_string" => {
                let mut out = String::new();
                for child in node.named_children(&mut node.walk()) {
                    let value = self.evaluate_expression(child).await?;
                    out.push_str(&value.as_string());
                }
                return Ok(PhpValue::String(out));
            }
            "string_content" => {
                let source = self.source_code.as_bytes();
                let raw = node
                    .utf8_text(source)
                    .map_err(|_| anyhow!("Failed to get text"))?;
                return Ok(PhpValue::String(raw.to_string()));
            }
            "escape_sequence" => {
                let source = self.source_code.as_bytes();
                let raw = node
                    .utf8_text(source)
                    .map_err(|_| anyhow!("Failed to get text"))?;
                if raw.contains("$") {
                    let cleaned = raw.trim_matches(|c| c == '{' || c == '}' || c == '$');
                    if let Some(value) = self.variables.get(cleaned) {
                        return Ok(value.clone());
                    }
                }
                return Ok(PhpValue::String(self.unescape_sequence(raw)));
            }
            "variable_name" => {
                if let Some(id) = self.get_identifier(node) {
                    if let Some(superglobal) = self.superglobals.get(&id) {
                        let mut items = Vec::new();
                        for (key, value) in superglobal {
                            items.push(PhpArrayItem::KeyValue(
                                key.clone(),
                                PhpValue::String(value.clone()),
                            ));
                        }
                        return Ok(PhpValue::Array(items));
                    }
                    if let Some(value) = self.variables.get(&id) {
                        return Ok(value.clone());
                    }
                    return Ok(PhpValue::Null);
                }
                return Ok(PhpValue::Null);
            }
            "name" => {
                if let Some(id) = self.get_identifier(node) {
                    // Magic constants
                    match id.as_str() {
                        "__DIR__" => {
                            return Ok(PhpValue::String(
                                self.current_template_path
                                    .as_ref()
                                    .and_then(|p| p.parent().map(|pp| pp.to_path_buf()))
                                    .map_or(String::new(), |p| p.to_string_lossy().to_string()),
                            ));
                        }
                        "__FILE__" => {
                            return Ok(PhpValue::String(
                                self.current_template_path
                                    .as_ref()
                                    .map_or(String::new(), |p| p.to_string_lossy().to_string()),
                            ));
                        }
                        "__LINE__" => {
                            return Ok(PhpValue::Int(node.start_position().row as i64 + 1))
                        }
                        _ => {}
                    }
                    if let Some(val) = self.constants.get(&id) {
                        return Ok(val.clone());
                    }
                    return Ok(PhpValue::String(id));
                }
                return Ok(PhpValue::Null);
            }
            "single_quoted_string" | "double_quoted_string" => {
                let text = node
                    .utf8_text(self.source_code.as_bytes())
                    .map_err(|_| anyhow!("Failed to get text"))?;
                return Ok(PhpValue::String(
                    text.trim_matches(|c| c == '\'' || c == '"').to_string(),
                ));
            }
            "string" => {
                let text = node
                    .utf8_text(self.source_code.as_bytes())
                    .map_err(|_| anyhow!("Failed to get text"))?;
                Ok(PhpValue::String(
                    text.trim_matches(|c| c == '\'' || c == '"').to_string(),
                ))
            }
            "integer" => {
                let text = node
                    .utf8_text(self.source_code.as_bytes())
                    .map_err(|_| anyhow!("Failed to get number"))?;
                Ok(PhpValue::Int(text.parse::<i64>().unwrap_or(0)))
            }
            "float" => {
                let text = node
                    .utf8_text(self.source_code.as_bytes())
                    .map_err(|_| anyhow!("Failed to get number"))?;
                Ok(PhpValue::Float(text.parse::<f64>().unwrap_or(0.0)))
            }
            "variable" => {
                let var_name = node
                    .utf8_text(self.source_code.as_bytes())?
                    .trim_start_matches('$')
                    .to_string();
                if let Some(value) = self.variables.get(&var_name) {
                    Ok(value.clone())
                } else if let Some(superglobal) = self.superglobals.get(&var_name) {
                    let mut items = Vec::new();
                    for (key, value) in superglobal {
                        items.push(PhpArrayItem::KeyValue(
                            key.clone(),
                            PhpValue::String(value.clone()),
                        ));
                    }
                    Ok(PhpValue::Array(items))
                } else {
                    Ok(PhpValue::Null)
                }
            }
            "subscript_expression" => {
                let target = node
                    .child_by_field_name("value")
                    .or_else(|| node.child_by_field_name("array"))
                    .or_else(|| node.named_child(0));
                let index = node
                    .child_by_field_name("index")
                    .or_else(|| node.child_by_field_name("offset"))
                    .or_else(|| node.named_child(1));
                if let (Some(target), Some(index)) = (target, index) {
                    let key = self.evaluate_expression(index).await?.as_string();
                    // Evaluate target as an expression — handles chains like $a['b']['c'],
                    // function_call()['key'], etc. Superglobals are returned as Array by
                    // the variable handler so they work transparently here.
                    let target_val = self.evaluate_expression(target).await?;
                    if let PhpValue::Array(items) = target_val {
                        if let Ok(idx) = key.parse::<usize>() {
                            if let Some(item) = items.get(idx) {
                                match item {
                                    PhpArrayItem::KeyValue(_, value) => return Ok(value.clone()),
                                    PhpArrayItem::Value(value) => return Ok(value.clone()),
                                }
                            }
                        }
                        for item in &items {
                            if let PhpArrayItem::KeyValue(item_key, value) = item {
                                if item_key == &key {
                                    return Ok(value.clone());
                                }
                            }
                        }
                    }
                }
                Ok(PhpValue::Null)
            }
            "binary_expression" => {
                let left = node
                    .child_by_field_name("left")
                    .or_else(|| node.named_child(0));
                let right = node
                    .child_by_field_name("right")
                    .or_else(|| node.named_child(1));
                let operator = {
                    let source = self.source_code.as_bytes();
                    node.child_by_field_name("operator")
                        .and_then(|op| op.utf8_text(source).ok())
                        .unwrap_or_default()
                        .to_string()
                };
                if let (Some(left), Some(right)) = (left, right) {
                    // Short-circuit for logical operators
                    if operator == "&&" || operator == "and" {
                        let left_val = self.evaluate_expression(left).await?;
                        if !left_val.is_truthy() {
                            return Ok(PhpValue::Bool(false));
                        }
                        let right_val = self.evaluate_expression(right).await?;
                        return Ok(PhpValue::Bool(right_val.is_truthy()));
                    }
                    if operator == "||" || operator == "or" {
                        let left_val = self.evaluate_expression(left).await?;
                        if left_val.is_truthy() {
                            return Ok(PhpValue::Bool(true));
                        }
                        let right_val = self.evaluate_expression(right).await?;
                        return Ok(PhpValue::Bool(right_val.is_truthy()));
                    }
                    if operator == "??" {
                        let left_val = self.evaluate_expression(left).await?;
                        if !left_val.is_null() {
                            return Ok(left_val);
                        }
                        return self.evaluate_expression(right).await;
                    }

                    let left_val = self.evaluate_expression(left).await?;
                    let right_val = self.evaluate_expression(right).await?;

                    match operator.as_str() {
                        "." => Ok(PhpValue::String(format!(
                            "{}{}",
                            left_val.as_string(),
                            right_val.as_string()
                        ))),
                        // Comparison
                        "==" => Ok(PhpValue::Bool(left_val.loose_eq(&right_val))),
                        "!=" | "<>" => Ok(PhpValue::Bool(!left_val.loose_eq(&right_val))),
                        "===" => Ok(PhpValue::Bool(left_val.strict_eq(&right_val))),
                        "!==" => Ok(PhpValue::Bool(!left_val.strict_eq(&right_val))),
                        "<" => Ok(PhpValue::Bool(left_val.as_float() < right_val.as_float())),
                        ">" => Ok(PhpValue::Bool(left_val.as_float() > right_val.as_float())),
                        "<=" => Ok(PhpValue::Bool(left_val.as_float() <= right_val.as_float())),
                        ">=" => Ok(PhpValue::Bool(left_val.as_float() >= right_val.as_float())),
                        // Arithmetic — preserve Int when both operands are int-like
                        "+" => Ok(if left_val.is_int_like() && right_val.is_int_like() {
                            PhpValue::Int(left_val.as_int().wrapping_add(right_val.as_int()))
                        } else {
                            PhpValue::Float(left_val.as_float() + right_val.as_float())
                        }),
                        "-" => Ok(if left_val.is_int_like() && right_val.is_int_like() {
                            PhpValue::Int(left_val.as_int().wrapping_sub(right_val.as_int()))
                        } else {
                            PhpValue::Float(left_val.as_float() - right_val.as_float())
                        }),
                        "*" => Ok(if left_val.is_int_like() && right_val.is_int_like() {
                            PhpValue::Int(left_val.as_int().wrapping_mul(right_val.as_int()))
                        } else {
                            PhpValue::Float(left_val.as_float() * right_val.as_float())
                        }),
                        "/" => {
                            let d = right_val.as_float();
                            if d == 0.0 {
                                self.log_error("Division by zero");
                                Ok(PhpValue::Bool(false))
                            } else if left_val.is_int_like() && right_val.is_int_like() {
                                let li = left_val.as_int();
                                let ri = right_val.as_int();
                                if ri != 0 && li % ri == 0 {
                                    Ok(PhpValue::Int(li / ri))
                                } else {
                                    Ok(PhpValue::Float(left_val.as_float() / d))
                                }
                            } else {
                                Ok(PhpValue::Float(left_val.as_float() / d))
                            }
                        }
                        "%" => {
                            let d = right_val.as_int();
                            if d == 0 {
                                Ok(PhpValue::Bool(false))
                            } else {
                                Ok(PhpValue::Int(left_val.as_int() % d))
                            }
                        }
                        "**" => Ok(PhpValue::Float(
                            left_val.as_float().powf(right_val.as_float()),
                        )),
                        // Bitwise
                        "&" => Ok(PhpValue::Int(left_val.as_int() & right_val.as_int())),
                        "|" => Ok(PhpValue::Int(left_val.as_int() | right_val.as_int())),
                        "^" => Ok(PhpValue::Int(left_val.as_int() ^ right_val.as_int())),
                        "<<" => Ok(PhpValue::Int(left_val.as_int() << right_val.as_int())),
                        ">>" => Ok(PhpValue::Int(left_val.as_int() >> right_val.as_int())),
                        "instanceof" => {
                            let is_obj = matches!(left_val, PhpValue::Object(_));
                            if !is_obj {
                                return Ok(PhpValue::Bool(false));
                            }
                            let class_name = right_val.as_string();
                            if let PhpValue::Object(obj) = &left_val {
                                // Check direct class name; walk parent chain for inheritance
                                let mut search = Some(obj.class_name.clone());
                                let mut matched = false;
                                while let Some(ref cn) = search.clone() {
                                    if cn == &class_name {
                                        matched = true;
                                        break;
                                    }
                                    search = self
                                        .user_classes
                                        .get(cn)
                                        .and_then(|c| c.parent.clone());
                                }
                                Ok(PhpValue::Bool(matched))
                            } else {
                                Ok(PhpValue::Bool(false))
                            }
                        }
                        _ => Ok(PhpValue::Null),
                    }
                } else {
                    Ok(PhpValue::Null)
                }
            }
            // ── unary operators ──────────────────────────────────────────
            "unary_op_expression" => {
                let operand = node.named_child(0);
                let operator = {
                    let source = self.source_code.as_bytes();
                    let mut op = String::new();
                    for i in 0..node.child_count() {
                        if let Some(child) = node.child(i) {
                            if !child.is_named() {
                                op = child.utf8_text(source).unwrap_or("").to_string();
                                break;
                            }
                        }
                    }
                    op
                };
                if let Some(operand) = operand {
                    let val = self.evaluate_expression(operand).await?;
                    match operator.as_str() {
                        "!" => Ok(PhpValue::Bool(!val.is_truthy())),
                        "-" => Ok(if val.is_int_like() {
                            PhpValue::Int(-val.as_int())
                        } else {
                            PhpValue::Float(-val.as_float())
                        }),
                        "+" => Ok(if val.is_int_like() {
                            PhpValue::Int(val.as_int())
                        } else {
                            PhpValue::Float(val.as_float())
                        }),
                        "~" => Ok(PhpValue::Int(!val.as_int())),
                        _ => Ok(val),
                    }
                } else {
                    Ok(PhpValue::Null)
                }
            }
            // ── ternary / conditional ────────────────────────────────────
            "conditional_expression" | "ternary_expression" => {
                let condition_node = node
                    .child_by_field_name("condition")
                    .or_else(|| node.named_child(0));
                // Use field names ONLY for body/alternative to distinguish
                // full ternary ($a ? $b : $c) from short ternary ($a ?: $c).
                // With short ternary, tree-sitter has no "body" field.
                let if_true = node.child_by_field_name("body");
                let if_false = node.child_by_field_name("alternative");

                if let Some(condition) = condition_node {
                    let cond_val = self.evaluate_expression(condition).await?;
                    if cond_val.is_truthy() {
                        if let Some(if_true) = if_true {
                            return self.evaluate_expression(if_true).await;
                        }
                        // Short ternary: $a ?: $b — return $a when truthy
                        return Ok(cond_val);
                    } else if let Some(if_false) = if_false {
                        return self.evaluate_expression(if_false).await;
                    }
                }
                Ok(PhpValue::Null)
            }
            // ── cast expressions ─────────────────────────────────────────
            "cast_expression" => {
                let cast_type = node
                    .child_by_field_name("type")
                    .and_then(|n| n.utf8_text(self.source_code.as_bytes()).ok())
                    .unwrap_or("")
                    .to_string();
                let value = if let Some(operand) = node
                    .child_by_field_name("value")
                    .or_else(|| node.named_child(0))
                {
                    self.evaluate_expression(operand).await?
                } else {
                    PhpValue::Null
                };
                match cast_type.trim().to_lowercase().as_str() {
                    "int" | "integer" => Ok(PhpValue::Int(value.as_int())),
                    "float" | "double" | "real" => Ok(PhpValue::Float(value.as_float())),
                    "string" => {
                        // Special handling for SimpleXMLElement to string conversion
                        if let PhpValue::Object(obj) = &value {
                            if obj.class_name == "SimpleXMLElement" {
                                return Ok(PhpValue::String(xml_parser::get_xml_text(&value)));
                            }
                        }
                        Ok(PhpValue::String(value.as_string()))
                    }
                    "bool" | "boolean" => Ok(PhpValue::Bool(value.is_truthy())),
                    "array" => Ok(match value {
                        PhpValue::Array(_) => value,
                        _ => PhpValue::Array(vec![PhpArrayItem::Value(value)]),
                    }),
                    _ => Ok(value),
                }
            }
            // ── update expressions (++$i, $i++) ──────────────────────────
            "update_expression" => {
                // Find the variable and whether it's prefix or postfix
                let source = self.source_code.as_bytes();
                let raw = node.utf8_text(source)?.to_string();
                let is_increment = raw.contains("++");
                if let Some(var_node) = node.named_child(0) {
                    let val = self.evaluate_expression(var_node).await?;
                    let new_val = match &val {
                        PhpValue::Float(f) => {
                            if is_increment {
                                PhpValue::Float(f + 1.0)
                            } else {
                                PhpValue::Float(f - 1.0)
                            }
                        }
                        _ => {
                            if is_increment {
                                PhpValue::Int(val.as_int() + 1)
                            } else {
                                PhpValue::Int(val.as_int() - 1)
                            }
                        }
                    };
                    let is_prefix = raw.starts_with("++") || raw.starts_with("--");
                    self.assign_to(var_node, new_val.clone()).await?;
                    if is_prefix {
                        Ok(new_val)
                    } else {
                        Ok(val)
                    }
                } else {
                    Ok(PhpValue::Null)
                }
            }
            // ── boolean literals ─────────────────────────────────────────
            "boolean" | "true" | "false" => {
                let text = node.utf8_text(self.source_code.as_bytes())?.to_lowercase();
                Ok(PhpValue::Bool(text == "true"))
            }
            "null" => Ok(PhpValue::Null),
            "text_interpolation" => {
                let mut out = String::new();
                for child in node.named_children(&mut node.walk()) {
                    let value = self.evaluate_expression(child).await?;
                    out.push_str(&value.as_string());
                }
                return Ok(PhpValue::String(out));
            }
            "parenthesized_expression" => {
                if let Some(inner) = node.named_child(0) {
                    return self.evaluate_expression(inner).await;
                }
                Ok(PhpValue::Null)
            }
            "array_creation_expression" => {
                let mut items = Vec::new();
                for child in node.named_children(&mut node.walk()) {
                    if child.kind() == "array_element_initializer" {
                        let named_count = child.named_child_count();
                        if named_count >= 2 {
                            // key => value pair: first named child is key, second is value
                            let key_node = child.named_child(0).unwrap();
                            let value_node = child.named_child(1).unwrap();
                            let key = self.evaluate_expression(key_node).await?.as_string();
                            let value = self.evaluate_expression(value_node).await?;
                            items.push(PhpArrayItem::KeyValue(key, value));
                        } else if named_count == 1 {
                            // plain value (no key)
                            let value_node = child.named_child(0).unwrap();
                            let value = self.evaluate_expression(value_node).await?;
                            items.push(PhpArrayItem::Value(value));
                        }
                    }
                }
                Ok(PhpValue::Array(items))
            }
            "function_call_expression" => self.evaluate_function_call(node).await,
            // Assignment as expression: $a = $b = 5 returns 5
            "assignment_expression" => {
                let left = node.child_by_field_name("left");
                let right = node.child_by_field_name("right");
                if let (Some(left), Some(right)) = (left, right) {
                    let value = self.evaluate_expression(right).await?;
                    self.assign_to(left, value.clone()).await?;
                    Ok(value)
                } else {
                    Ok(PhpValue::Null)
                }
            }
            // @ error suppression: evaluate the inner expression, swallow errors
            "error_suppression_expression" => {
                if let Some(inner) = node.named_child(0) {
                    match self.evaluate_expression(inner).await {
                        Ok(val) => Ok(val),
                        Err(_) => Ok(PhpValue::Null),
                    }
                } else {
                    Ok(PhpValue::Null)
                }
            }
            // require/include as expression: $config = require 'file.php'
            "include_expression"
            | "require_expression"
            | "include_once_expression"
            | "require_once_expression" => {
                let is_once = node.kind().contains("_once");
                let target = node
                    .child_by_field_name("path")
                    .or_else(|| node.child_by_field_name("argument"))
                    .or_else(|| node.named_child(0));
                if let Some(target) = target {
                    let value = self.evaluate_expression(target).await?;
                    let (include_output, return_value) =
                        self.include_file(&value.as_string(), is_once).await?;
                    if !include_output.is_empty() {
                        self.side_effect_output = Some(include_output);
                    }
                    // Return the script's PHP return value, or Bool(true) if it didn't return
                    Ok(return_value.unwrap_or(PhpValue::Bool(true)))
                } else {
                    Ok(PhpValue::Bool(false))
                }
            }
            // augmented assignment as expression (e.g. inside function body re-parsed)
            "augmented_assignment_expression" => {
                let left = node.child_by_field_name("left");
                let right = node.child_by_field_name("right");
                let operator = {
                    let source = self.source_code.as_bytes();
                    let mut op = String::new();
                    for i in 0..node.child_count() {
                        if let Some(child) = node.child(i) {
                            if !child.is_named() {
                                let text = child.utf8_text(source).unwrap_or("");
                                if text.ends_with('=') && text.len() >= 2 {
                                    op = text.to_string();
                                    break;
                                }
                            }
                        }
                    }
                    op
                };
                if let (Some(left), Some(right)) = (left, right) {
                    let left_val = self.evaluate_expression(left).await?;
                    let right_val = self.evaluate_expression(right).await?;
                    let result = match operator.as_str() {
                        "+=" => {
                            if left_val.is_int_like() && right_val.is_int_like() {
                                PhpValue::Int(left_val.as_int().wrapping_add(right_val.as_int()))
                            } else {
                                PhpValue::Float(left_val.as_float() + right_val.as_float())
                            }
                        }
                        "-=" => {
                            if left_val.is_int_like() && right_val.is_int_like() {
                                PhpValue::Int(left_val.as_int().wrapping_sub(right_val.as_int()))
                            } else {
                                PhpValue::Float(left_val.as_float() - right_val.as_float())
                            }
                        }
                        "*=" => {
                            if left_val.is_int_like() && right_val.is_int_like() {
                                PhpValue::Int(left_val.as_int().wrapping_mul(right_val.as_int()))
                            } else {
                                PhpValue::Float(left_val.as_float() * right_val.as_float())
                            }
                        }
                        "/=" => {
                            let d = right_val.as_float();
                            if d == 0.0 {
                                PhpValue::Bool(false)
                            } else if left_val.is_int_like() && right_val.is_int_like() {
                                let li = left_val.as_int();
                                let ri = right_val.as_int();
                                if ri != 0 && li % ri == 0 {
                                    PhpValue::Int(li / ri)
                                } else {
                                    PhpValue::Float(left_val.as_float() / d)
                                }
                            } else {
                                PhpValue::Float(left_val.as_float() / d)
                            }
                        }
                        ".=" => PhpValue::String(format!(
                            "{}{}",
                            left_val.as_string(),
                            right_val.as_string()
                        )),
                        "??=" => {
                            if left_val.is_null() {
                                right_val
                            } else {
                                left_val
                            }
                        }
                        _ => right_val,
                    };
                    self.assign_to(left, result.clone()).await?;
                    Ok(result)
                } else {
                    Ok(PhpValue::Null)
                }
            }
            // ── object creation: new Foo($args) ────────────────────────
            "object_creation_expression" => {
                let class_node = node
                    .child_by_field_name("class")
                    .or_else(|| node.named_child(0));
                let Some(class_node) = class_node else {
                    return Ok(PhpValue::Null);
                };

                // Resolve class name — handle `new self` / `new static`
                let raw_class = class_node
                    .utf8_text(self.source_code.as_bytes())
                    .unwrap_or("")
                    .trim_start_matches('\\')
                    .to_string();
                let class_name = if raw_class == "self" || raw_class == "static" {
                    // Use current $this class if inside a method
                    if let Some(PhpValue::Object(this)) = self.variables.get("this") {
                        this.class_name.clone()
                    } else {
                        raw_class
                    }
                } else {
                    raw_class
                };

                let class_def = self.user_classes.get(&class_name).cloned();
                let Some(class_def) = class_def else {
                    self.log_error_with_context(&format!("Class '{}' not found", class_name));
                    return Ok(PhpValue::Null);
                };

                // Initialise properties: parent defaults first, then child
                let mut props = if let Some(ref parent_name) = class_def.parent {
                    self.user_classes
                        .get(parent_name)
                        .map(|p| p.default_props.clone())
                        .unwrap_or_default()
                } else {
                    HashMap::new()
                };
                props.extend(class_def.default_props.clone());

                // Evaluate constructor arguments
                let mut args_node = node.child_by_field_name("arguments");
                if args_node.is_none() {
                    // Fallback: tree-sitter-php may not expose args as a named field
                    for child in node.named_children(&mut node.walk()) {
                        if child.kind() == "arguments" || child.kind() == "argument_list" {
                            args_node = Some(child);
                            break;
                        }
                    }
                }
                let mut args = Vec::new();
                if let Some(args_node) = args_node {
                    for child in args_node.named_children(&mut args_node.walk()) {
                        if child.kind() == "argument" {
                            if let Some(inner) = child.named_child(0) {
                                args.push(self.evaluate_expression(inner).await?);
                                continue;
                            }
                        }
                        args.push(self.evaluate_expression(child).await?);
                    }
                }

                let mut obj = PhpObject { class_name: class_name.clone(), props };

                // Call __construct if defined
                if class_def.methods.contains_key("__construct") {
                    debug!("[{}] PHP new {} — calling __construct with {} args", self.request_domain, class_name, args.len());
                    let (_, updated) = self.call_method(obj, "__construct", args).await?;
                    obj = updated;
                } else if obj.props.contains_key("message") && !args.is_empty() {
                    // Built-in exception-style constructor: ($message, $code, $previous)
                    if let Some(msg) = args.first() {
                        debug!("[{}] PHP new {} — built-in exception ctor, message={:?}", self.request_domain, class_name, msg);
                        obj.props.insert("message".to_string(), msg.clone());
                    }
                    if let Some(code) = args.get(1) {
                        obj.props.insert("code".to_string(), code.clone());
                    }
                } else {
                    debug!("[{}] PHP new {} — no __construct, {} args, has_message_prop={}", self.request_domain, class_name, args.len(), obj.props.contains_key("message"));
                }

                Ok(PhpValue::Object(Box::new(obj)))
            }

            // ── property read: $obj->prop ───────────────────────────────
            "member_access_expression" | "nullsafe_member_access_expression" => {
                let is_nullsafe = node.kind() == "nullsafe_member_access_expression";
                let obj_node = node
                    .child_by_field_name("object")
                    .or_else(|| node.named_child(0));
                let name_node = node
                    .child_by_field_name("name")
                    .or_else(|| node.child_by_field_name("member"))
                    .or_else(|| node.named_child(1));
                let Some(obj_node) = obj_node else {
                    return Ok(PhpValue::Null);
                };
                let obj_val = self.evaluate_expression(obj_node).await?;
                match obj_val {
                    PhpValue::Null if is_nullsafe => Ok(PhpValue::Null),
                    PhpValue::Object(obj) => {
                        if let Some(name_node) = name_node {
                            let prop = name_node
                                .utf8_text(self.source_code.as_bytes())
                                .unwrap_or("")
                                .trim_start_matches('$')
                                .to_string();

                            // Special handling for SimpleXMLElement
                            if obj.class_name == "SimpleXMLElement" {
                                Ok(xml_parser::get_xml_child(&PhpValue::Object(obj), &prop))
                            } else {
                                Ok(obj.props.get(&prop).cloned().unwrap_or(PhpValue::Null))
                            }
                        } else {
                            Ok(PhpValue::Null)
                        }
                    }
                    _ => {
                        if !is_nullsafe {
                            self.log_error("Property access on non-object");
                        }
                        Ok(PhpValue::Null)
                    }
                }
            }

            // ── method call: $obj->method($args) ───────────────────────
            "member_call_expression" | "nullsafe_member_call_expression" => {
                let is_nullsafe = node.kind() == "nullsafe_member_call_expression";
                let obj_node = node
                    .child_by_field_name("object")
                    .or_else(|| node.named_child(0));
                let name_node = node
                    .child_by_field_name("name")
                    .or_else(|| node.child_by_field_name("member"))
                    .or_else(|| node.named_child(1));
                let args_node = node.child_by_field_name("arguments");
                let Some(obj_node) = obj_node else {
                    return Ok(PhpValue::Null);
                };

                let obj_val = self.evaluate_expression(obj_node).await?;
                let method_name = if let Some(nn) = name_node {
                    nn.utf8_text(self.source_code.as_bytes())
                        .unwrap_or("")
                        .to_lowercase()
                } else {
                    return Ok(PhpValue::Null);
                };
                match obj_val {
                    PhpValue::Null if is_nullsafe => Ok(PhpValue::Null),
                    PhpValue::Object(obj_box) => {
                        let obj = *obj_box;

                        let mut args = Vec::new();
                        if let Some(args_node) = args_node {
                            for child in args_node.named_children(&mut args_node.walk()) {
                                if child.kind() == "argument" {
                                    if let Some(inner) = child.named_child(0) {
                                        args.push(self.evaluate_expression(inner).await?);
                                        continue;
                                    }
                                }
                                args.push(self.evaluate_expression(child).await?);
                            }
                        }

                        let (return_val, updated_obj) =
                            self.call_method(obj, &method_name, args).await?;

                        // Write the updated object back to its source variable
                        if let Some(var_name) = self.get_identifier(obj_node) {
                            self.variables.insert(
                                var_name,
                                PhpValue::Object(Box::new(updated_obj)),
                            );
                        }
                        Ok(return_val)
                    }
                    other => {
                        if !is_nullsafe {
                            let line = node.start_position().row + 1;
                            let obj_src = obj_node.utf8_text(self.source_code.as_bytes()).unwrap_or("?");
                            let type_name = match &other {
                                PhpValue::Null => "null",
                                PhpValue::String(_) => "string",
                                PhpValue::Int(_) => "int",
                                PhpValue::Float(_) => "float",
                                PhpValue::Bool(_) => "bool",
                                PhpValue::Array(_) => "array",
                                PhpValue::Resource(_) => "resource",
                                PhpValue::Object(_) => "object",
                            };
                            warn!("[{}] {} Method call on non-object ({}) for {}->{} at line {}", self.request_domain, self.request_uri, type_name, obj_src, method_name, line);
                            self.log_error_with_context(&format!(
                                "Method call on non-object ({}) for {}->{} at line {}",
                                type_name, obj_src, method_name, line
                            ));
                        }
                        Ok(PhpValue::Null)
                    }
                }
            }

            // ── throw expression: throw new Exception(...) ────────
            "throw_expression" => {
                let expr = node.named_child(0);
                let val = if let Some(expr) = expr {
                    self.evaluate_expression(expr).await?
                } else {
                    PhpValue::Null
                };
                let (class_name, message) = match val {
                    PhpValue::Object(obj) => {
                        let msg = obj.props.get("message").map_or(String::new(), |v| v.as_string());
                        debug!("[{}] PHP throw expr {}: props={:?}, msg='{}'", self.request_domain, obj.class_name, obj.props.keys().collect::<Vec<_>>(), msg);
                        (obj.class_name.clone(), msg)
                    }
                    _ => {
                        debug!("[{}] PHP throw expr (non-object): {:?}", self.request_domain, val);
                        ("Exception".to_string(), val.as_string())
                    }
                };
                Err(anyhow!(PhpThrow { class_name, message }))
            }

            _ => {
                let kind = node.kind();
                let line = node.start_position().row + 1;
                warn!("[{}] {} Unhandled expression '{}' at line {}", self.request_domain, self.request_uri, kind, line);
                self.log_error_with_context(&format!(
                    "Unhandled expression node '{}' at line {}",
                    kind, line
                ));
                // Do NOT call process_node here: any node ending in `_expression`
                // would cause process_node to call back into evaluate_expression,
                // creating infinite mutual recursion and a stack overflow.
                Ok(PhpValue::Null)
            }
        }
    }

    async fn evaluate_function_call(&mut self, node: Node<'_>) -> Result<PhpValue> {
        let function = node
            .child_by_field_name("function")
            .ok_or_else(|| anyhow!("Function node missing"))?;
        let mut args_node = node.child_by_field_name("arguments");
        if args_node.is_none() {
            for child in node.named_children(&mut node.walk()) {
                if child.kind() == "arguments" || child.kind() == "argument_list" {
                    args_node = Some(child);
                    break;
                }
            }
        }
        let func_name = {
            let source = self.source_code.as_bytes();
            let raw = function.utf8_text(source)?.to_string();
            // Strip leading backslash from namespace-qualified calls like \is_scalar()
            raw.trim_start_matches('\\').to_string()
        };

        // Handle preg_match / preg_match_all — 3rd arg is a by-ref output variable
        if func_name == "preg_match" || func_name == "preg_match_all" {
            return self.call_preg_match_family(&func_name, args_node).await;
        }

        // Handle unset() specially — needs raw AST nodes to find variable/key references
        if func_name == "unset" {
            if let Some(args_node) = args_node {
                for child in args_node.named_children(&mut args_node.walk()) {
                    let target = if child.kind() == "argument" {
                        child.named_child(0).unwrap_or(child)
                    } else {
                        child
                    };
                    self.unset_target(target).await?;
                }
            }
            return Ok(PhpValue::Null);
        }

        // Handle array_map / array_filter with callback (arrow_function / anonymous_function)
        if func_name == "array_map" || func_name == "array_filter" {
            if let Some(args_node) = args_node {
                return self.call_array_callback_function(&func_name, args_node).await;
            }
        }

        let mut args = Vec::new();
        if let Some(args_node) = args_node {
            for child in args_node.named_children(&mut args_node.walk()) {
                if child.kind() == "argument" {
                    if let Some(inner) = child.named_child(0) {
                        args.push(self.evaluate_expression(inner).await?);
                        continue;
                    }
                }
                args.push(self.evaluate_expression(child).await?);
            }
        }

        match func_name.as_str() {
            "phpversion" => Ok(PhpValue::String("8.4.0-ast".to_string())),
            "date" => {
                let fmt = args.first().map_or(String::new(), |a| a.as_string());
                let dt = if let Some(ts) = args.get(1) {
                    let secs = ts.as_int();
                    chrono::DateTime::from_timestamp(secs, 0)
                        .map(|dt| dt.with_timezone(&chrono::Local))
                        .unwrap_or_else(chrono::Local::now)
                } else {
                    chrono::Local::now()
                };
                if fmt.is_empty() {
                    return Ok(PhpValue::String(dt.format("%Y-%m-%d %H:%M:%S").to_string()));
                }
                let mut out = String::new();
                let chars: Vec<char> = fmt.chars().collect();
                let mut i = 0;
                while i < chars.len() {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        out.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    match chars[i] {
                        'Y' => out.push_str(&dt.format("%Y").to_string()),
                        'y' => out.push_str(&dt.format("%y").to_string()),
                        'm' => out.push_str(&dt.format("%m").to_string()),
                        'n' => out.push_str(&dt.format("%-m").to_string()),
                        'd' => out.push_str(&dt.format("%d").to_string()),
                        'j' => out.push_str(&dt.format("%-d").to_string()),
                        'H' => out.push_str(&dt.format("%H").to_string()),
                        'G' => out.push_str(&dt.format("%-H").to_string()),
                        'i' => out.push_str(&dt.format("%M").to_string()),
                        's' => out.push_str(&dt.format("%S").to_string()),
                        'A' => out.push_str(&dt.format("%p").to_string()),
                        'a' => out.push_str(&dt.format("%P").to_string()),
                        'g' => out.push_str(&dt.format("%-I").to_string()),
                        'U' => out.push_str(&dt.timestamp().to_string()),
                        'N' => out.push_str(&dt.format("%u").to_string()),
                        'w' => out.push_str(&dt.format("%w").to_string()),
                        'D' => out.push_str(&dt.format("%a").to_string()),
                        'l' => out.push_str(&dt.format("%A").to_string()),
                        'F' => out.push_str(&dt.format("%B").to_string()),
                        'M' => out.push_str(&dt.format("%b").to_string()),
                        't' => {
                            let y = dt.format("%Y").to_string().parse::<i32>().unwrap_or(2000);
                            let m = dt.format("%m").to_string().parse::<u32>().unwrap_or(1);
                            let days = match m {
                                1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
                                4 | 6 | 9 | 11 => 30,
                                2 => {
                                    if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
                                        29
                                    } else {
                                        28
                                    }
                                }
                                _ => 30,
                            };
                            out.push_str(&days.to_string());
                        }
                        c => out.push(c),
                    }
                    i += 1;
                }
                Ok(PhpValue::String(out))
            }
            "header" => {
                if let Some(header_line) = args.get(0) {
                    let header_line = header_line.as_string();
                    if let Some((name, value)) = header_line.split_once(':') {
                        self.add_response_header(name, value.trim().to_string());
                    }
                }
                // 3rd arg: HTTP response code (e.g. header("Location: ...", true, 301))
                if let Some(code) = args.get(2) {
                    let code = code.as_int();
                    if (100..600).contains(&code) {
                        self.response_status = code as u16;
                    }
                }
                Ok(PhpValue::Null)
            }
            "response" => {
                if let Some(status) = args.get(0) {
                    if let Ok(status) = status.as_string().parse::<u16>() {
                        self.response_status = status;
                    }
                }
                if let Some(headers) = args.get(1) {
                    self.apply_header_block(headers);
                }
                if let Some(body) = args.get(2) {
                    self.response_body_override = Some(body.as_string());
                }
                Ok(PhpValue::Null)
            }
            "render" => {
                if let Some(template) = args.get(0) {
                    if let Some(rendered) = self.render_template(template, args.get(1)).await? {
                        return Ok(PhpValue::String(rendered));
                    }
                }
                Ok(PhpValue::Null)
            }
            "ob_start" => {
                self.output_buffers.push(String::new());
                Ok(PhpValue::Null)
            }
            "ob_get_clean" => {
                let buffer = self.output_buffers.pop().unwrap_or_default();
                Ok(PhpValue::String(buffer))
            }
            "file_get_contents" => {
                if let Some(target) = args.get(0) {
                    let headers = args.get(1);
                    return self.fetch_content(target, headers).await;
                }
                Ok(PhpValue::Null)
            }
            "var_dump" => {
                if args.is_empty() {
                    self.log_error(
                        "var_dump called with no args (parser did not capture arguments)",
                    );
                }
                let mut out = String::new();
                for (idx, arg) in args.iter().enumerate() {
                    if idx > 0 {
                        out.push('\n');
                    }
                    out.push_str(&arg.dump());
                }
                self.side_effect_output = Some(out);
                return Ok(PhpValue::Null);
            }
            "exe" => {
                // exe('path/') - resolve and execute like the web server does:
                // directory -> find _index.php -> execute it
                // file.php -> execute it
                // file.html -> process through PHP (handles <?php tags)
                if let Some(target) = args.get(0) {
                    let target_str = target.as_string();
                    let path = self.resolve_local_path(&target_str)?;

                    let script = if path.is_dir() {
                        // Look for _index.php in directory
                        let candidate = path.join("_index.php");
                        if candidate.exists() {
                            candidate
                        } else {
                            return Err(anyhow!("No _index.php found in {}", target_str));
                        }
                    } else if path.is_file() {
                        path
                    } else {
                        return Err(anyhow!("exe: path not found: {}", target_str));
                    };

                    let content = fs::read_to_string(&script)
                        .await
                        .map_err(|_| anyhow!("exe: cannot read {}", script.display()))?;

                    let previous_source = self.source_code.clone();
                    let previous_template = self.current_template_path.clone();
                    self.current_template_path = Some(script.clone());

                    let mut exe_output = String::new();
                    self.process_php_code(&content, &mut exe_output).await?;

                    self.source_code = previous_source;
                    self.current_template_path = previous_template;
                    return Ok(PhpValue::String(exe_output));
                }
                Ok(PhpValue::Null)
            }
            "http_request" => {
                let method = args
                    .get(0)
                    .map(|s| s.as_string())
                    .unwrap_or("GET".to_string());
                let url = args.get(1).map(|s| s.as_string()).unwrap_or_default();
                let headers = args.get(2);
                let body = args.get(3).map(|s| s.as_string()).unwrap_or_default();
                if !url.is_empty() {
                    return self.http_request(&method, &url, headers, &body).await;
                }
                Ok(PhpValue::Null)
            }

            // ── exit / die (as function call) ───────────────────────────
            "exit" | "die" => {
                let code = args
                    .first()
                    .map(|a| match a {
                        PhpValue::Int(n) => *n as i32,
                        PhpValue::String(s) => {
                            self.side_effect_output = Some(s.clone());
                            0
                        }
                        _ => 0,
                    })
                    .unwrap_or(0);
                return Err(anyhow!(PhpExit { code }));
            }

            // ── http_response_code ──────────────────────────────────────
            "http_response_code" => {
                if let Some(code) = args.first() {
                    let code_str = code.as_string();
                    debug!("[{}] PHP http_response_code: {}", self.request_domain, code_str);
                    if let Ok(status) = code_str.parse::<u16>() {
                        self.response_status = status;
                    }
                }
                Ok(PhpValue::Int(self.response_status as i64))
            }

            // ── readfile ────────────────────────────────────────────────
            "readfile" => {
                if let Some(target) = args.first() {
                    let path_str = target.as_string();
                    let path = self.resolve_local_path(&path_str)?;
                    let content = fs::read_to_string(&path)
                        .await
                        .map_err(|_| anyhow!("readfile: cannot read {}", path.display()))?;
                    self.side_effect_output = Some(content.clone());
                    return Ok(PhpValue::Int(content.len() as i64));
                }
                Ok(PhpValue::Bool(false))
            }

            // ── Type checking ───────────────────────────────────────────
            "isset" => Ok(PhpValue::Bool(args.iter().all(|a| !a.is_null()))),
            "empty" => Ok(PhpValue::Bool(
                args.first().map_or(true, |a| a.is_empty_value()),
            )),
            "is_null" => Ok(PhpValue::Bool(args.first().map_or(true, |a| a.is_null()))),
            "is_array" => Ok(PhpValue::Bool(matches!(
                args.first(),
                Some(PhpValue::Array(_))
            ))),
            "is_string" => Ok(PhpValue::Bool(matches!(
                args.first(),
                Some(PhpValue::String(_))
            ))),
            "is_numeric" => Ok(PhpValue::Bool(args.first().map_or(false, |a| {
                matches!(a, PhpValue::Int(_) | PhpValue::Float(_))
                    || matches!(a, PhpValue::String(s) if s.parse::<f64>().is_ok())
            }))),
            "is_int" | "is_integer" | "is_long" => Ok(PhpValue::Bool(matches!(
                args.first(),
                Some(PhpValue::Int(_))
            ))),
            "is_bool" => Ok(PhpValue::Bool(matches!(
                args.first(),
                Some(PhpValue::Bool(_))
            ))),
            "is_scalar" => Ok(PhpValue::Bool(matches!(
                args.first(),
                Some(
                    PhpValue::Int(_) | PhpValue::Float(_) | PhpValue::String(_) | PhpValue::Bool(_)
                )
            ))),
            "is_float" | "is_double" => Ok(PhpValue::Bool(matches!(
                args.first(),
                Some(PhpValue::Float(_))
            ))),
            "gettype" => {
                let t = match args.first() {
                    Some(PhpValue::Null) | None => "NULL",
                    Some(PhpValue::Bool(_)) => "boolean",
                    Some(PhpValue::Int(_)) => "integer",
                    Some(PhpValue::Float(_)) => "double",
                    Some(PhpValue::String(_)) => "string",
                    Some(PhpValue::Array(_)) => "array",
                    Some(PhpValue::Resource(_)) => "resource",
                    Some(PhpValue::Object(_)) => "object",
                };
                Ok(PhpValue::String(t.to_string()))
            }

            // ── String functions ─────────────────────────────────────────
            "strlen" => Ok(PhpValue::Int(
                args.first().map_or(0, |a| a.as_string().len() as i64),
            )),
            "strtolower" => Ok(PhpValue::String(
                args.first()
                    .map_or(String::new(), |a| a.as_string().to_lowercase()),
            )),
            "strtoupper" => Ok(PhpValue::String(
                args.first()
                    .map_or(String::new(), |a| a.as_string().to_uppercase()),
            )),
            "trim" => {
                let s = args.first().map_or(String::new(), |a| a.as_string());
                let chars = args.get(1).map(|a| a.as_string());
                Ok(PhpValue::String(match chars {
                    Some(c) => s.trim_matches(|ch: char| c.contains(ch)).to_string(),
                    None => s.trim().to_string(),
                }))
            }
            "ltrim" => {
                let s = args.first().map_or(String::new(), |a| a.as_string());
                let mask = args.get(1).map(|a| a.as_string());
                Ok(PhpValue::String(match mask {
                    Some(m) => {
                        let chars: Vec<char> = m.chars().collect();
                        s.trim_start_matches(|c: char| chars.contains(&c))
                            .to_string()
                    }
                    None => s.trim_start().to_string(),
                }))
            }
            "rtrim" | "chop" => {
                let s = args.first().map_or(String::new(), |a| a.as_string());
                let mask = args.get(1).map(|a| a.as_string());
                Ok(PhpValue::String(match mask {
                    Some(m) => {
                        let chars: Vec<char> = m.chars().collect();
                        s.trim_end_matches(|c: char| chars.contains(&c)).to_string()
                    }
                    None => s.trim_end().to_string(),
                }))
            }
            "substr" => {
                let s = args.first().map_or(String::new(), |a| a.as_string());
                let start = args.get(1).map_or(0, |a| a.as_int()) as usize;
                let len = args.get(2).map(|a| a.as_int() as usize);
                let chars: Vec<char> = s.chars().collect();
                let start = start.min(chars.len());
                let end = len.map_or(chars.len(), |l| (start + l).min(chars.len()));
                Ok(PhpValue::String(chars[start..end].iter().collect()))
            }
            "str_replace" => {
                let search = args.first().map_or(String::new(), |a| a.as_string());
                let replace = args.get(1).map_or(String::new(), |a| a.as_string());
                let subject = args.get(2).map_or(String::new(), |a| a.as_string());
                Ok(PhpValue::String(subject.replace(&search, &replace)))
            }
            "str_contains" => {
                let haystack = args.first().map_or(String::new(), |a| a.as_string());
                let needle = args.get(1).map_or(String::new(), |a| a.as_string());
                Ok(PhpValue::Bool(haystack.contains(&needle)))
            }
            "str_starts_with" => {
                let haystack = args.first().map_or(String::new(), |a| a.as_string());
                let needle = args.get(1).map_or(String::new(), |a| a.as_string());
                Ok(PhpValue::Bool(haystack.starts_with(&needle)))
            }
            "str_ends_with" => {
                let haystack = args.first().map_or(String::new(), |a| a.as_string());
                let needle = args.get(1).map_or(String::new(), |a| a.as_string());
                Ok(PhpValue::Bool(haystack.ends_with(&needle)))
            }
            "substr_count" => {
                let haystack = args.first().map_or(String::new(), |a| a.as_string());
                let needle = args.get(1).map_or(String::new(), |a| a.as_string());
                if needle.is_empty() {
                    Ok(PhpValue::Int(0))
                } else {
                    Ok(PhpValue::Int(haystack.matches(&needle).count() as i64))
                }
            }
            "strpos" | "stripos" => {
                let haystack = args.first().map_or(String::new(), |a| a.as_string());
                let needle = args.get(1).map_or(String::new(), |a| a.as_string());
                let (h, n) = if func_name == "stripos" {
                    (haystack.to_lowercase(), needle.to_lowercase())
                } else {
                    (haystack, needle)
                };
                match h.find(&n) {
                    Some(pos) => Ok(PhpValue::Int(pos as i64)),
                    None => Ok(PhpValue::Bool(false)),
                }
            }
            "strrpos" => {
                let haystack = args.first().map_or(String::new(), |a| a.as_string());
                let needle = args.get(1).map_or(String::new(), |a| a.as_string());
                match haystack.rfind(&needle) {
                    Some(pos) => Ok(PhpValue::Int(pos as i64)),
                    None => Ok(PhpValue::Bool(false)),
                }
            }
            "explode" => {
                let delimiter = args.first().map_or(String::new(), |a| a.as_string());
                let string = args.get(1).map_or(String::new(), |a| a.as_string());
                let limit = args.get(2).map(|a| a.as_int());
                let parts: Vec<&str> = match limit {
                    Some(n) if n > 0 => string.splitn(n as usize, &delimiter).collect(),
                    _ => string.split(&delimiter).collect(),
                };
                let items: Vec<PhpArrayItem> = parts
                    .into_iter()
                    .map(|s| PhpArrayItem::Value(PhpValue::String(s.to_string())))
                    .collect();
                Ok(PhpValue::Array(items))
            }
            "implode" | "join" => {
                let glue = args.first().map_or(String::new(), |a| a.as_string());
                let empty_arr = PhpValue::Array(vec![]);
                let pieces = args.get(1).unwrap_or(&empty_arr);
                if let PhpValue::Array(items) = pieces {
                    let strs: Vec<String> = items
                        .iter()
                        .map(|item| match item {
                            PhpArrayItem::KeyValue(_, v) => v.as_string(),
                            PhpArrayItem::Value(v) => v.as_string(),
                        })
                        .collect();
                    Ok(PhpValue::String(strs.join(&glue)))
                } else {
                    Ok(PhpValue::String(String::new()))
                }
            }
            "sprintf" => {
                // Basic sprintf: only handles %s, %d, %f, %% for now
                let fmt = args.first().map_or(String::new(), |a| a.as_string());
                let mut arg_idx = 1;
                let mut i = 0;
                let chars: Vec<char> = fmt.chars().collect();
                let mut out = String::new();
                while i < chars.len() {
                    if chars[i] == '%' && i + 1 < chars.len() {
                        // Parse optional flags, width, precision: %[0-][width][.precision][type]
                        let mut j = i + 1;
                        let mut pad_char = ' ';
                        let mut width: usize = 0;
                        let mut precision: Option<usize> = None;
                        let mut left_align = false;
                        // Flags
                        while j < chars.len() {
                            match chars[j] {
                                '0' if width == 0 && precision.is_none() => {
                                    pad_char = '0';
                                    j += 1;
                                }
                                '-' => {
                                    left_align = true;
                                    j += 1;
                                }
                                _ => break,
                            }
                        }
                        // Width
                        while j < chars.len() && chars[j].is_ascii_digit() {
                            width = width * 10 + (chars[j] as usize - '0' as usize);
                            j += 1;
                        }
                        // Precision
                        if j < chars.len() && chars[j] == '.' {
                            j += 1;
                            let mut p = 0usize;
                            while j < chars.len() && chars[j].is_ascii_digit() {
                                p = p * 10 + (chars[j] as usize - '0' as usize);
                                j += 1;
                            }
                            precision = Some(p);
                        }
                        // Type specifier
                        if j < chars.len() {
                            let formatted = match chars[j] {
                                's' => {
                                    let s =
                                        args.get(arg_idx).map_or(String::new(), |a| a.as_string());
                                    arg_idx += 1;
                                    if let Some(p) = precision {
                                        s[..s.len().min(p)].to_string()
                                    } else {
                                        s
                                    }
                                }
                                'd' => {
                                    let n = args.get(arg_idx).map_or(0, |a| a.as_int());
                                    arg_idx += 1;
                                    n.to_string()
                                }
                                'f' => {
                                    let f = args.get(arg_idx).map_or(0.0, |a| a.as_float());
                                    arg_idx += 1;
                                    let p = precision.unwrap_or(6);
                                    format!("{:.*}", p, f)
                                }
                                'x' => {
                                    let n = args.get(arg_idx).map_or(0, |a| a.as_int());
                                    arg_idx += 1;
                                    format!("{:x}", n)
                                }
                                'X' => {
                                    let n = args.get(arg_idx).map_or(0, |a| a.as_int());
                                    arg_idx += 1;
                                    format!("{:X}", n)
                                }
                                'o' => {
                                    let n = args.get(arg_idx).map_or(0, |a| a.as_int());
                                    arg_idx += 1;
                                    format!("{:o}", n)
                                }
                                'b' => {
                                    let n = args.get(arg_idx).map_or(0, |a| a.as_int());
                                    arg_idx += 1;
                                    format!("{:b}", n)
                                }
                                '%' => {
                                    i = j + 1;
                                    out.push('%');
                                    continue;
                                }
                                _ => {
                                    out.push('%');
                                    i = i + 1;
                                    continue;
                                }
                            };
                            // Apply width padding
                            if width > formatted.len() {
                                let padding = width - formatted.len();
                                if left_align {
                                    out.push_str(&formatted);
                                    for _ in 0..padding {
                                        out.push(' ');
                                    }
                                } else {
                                    for _ in 0..padding {
                                        out.push(pad_char);
                                    }
                                    out.push_str(&formatted);
                                }
                            } else {
                                out.push_str(&formatted);
                            }
                            i = j + 1;
                        } else {
                            out.push(chars[i]);
                            i += 1;
                        }
                    } else {
                        out.push(chars[i]);
                        i += 1;
                    }
                }
                Ok(PhpValue::String(out))
            }
            "nl2br" => {
                let s = args.first().map_or(String::new(), |a| a.as_string());
                Ok(PhpValue::String(s.replace('\n', "<br />\n")))
            }
            "htmlspecialchars" | "htmlentities" => {
                let s = args.first().map_or(String::new(), |a| a.as_string());
                Ok(PhpValue::String(
                    s.replace('&', "&amp;")
                        .replace('<', "&lt;")
                        .replace('>', "&gt;")
                        .replace('"', "&quot;")
                        .replace('\'', "&#039;"),
                ))
            }
            "htmlspecialchars_decode" | "html_entity_decode" => {
                let s = args.first().map_or(String::new(), |a| a.as_string());
                Ok(PhpValue::String(html_entity_decode_basic(&s)))
            }
            "http_build_query" => {
                // http_build_query($data, $numeric_prefix, $arg_separator, $enc_type)
                let data = args.first().unwrap_or(&PhpValue::Null);
                let _numeric_prefix = args.get(1).map_or(String::new(), |a| a.as_string());
                let arg_separator = args.get(2).map(|a| a.as_string());
                let sep = arg_separator.as_deref().unwrap_or("&");
                // $enc_type: PHP_QUERY_RFC3986 (2) uses rawurlencode (percent-encoding with %20)
                // PHP_QUERY_RFC1738 (1, default) uses urlencode (+ for spaces)
                let _enc_type = args.get(3).map_or(1, |a| a.as_int());
                match data {
                    PhpValue::Array(items) => {
                        let pairs: Vec<String> = items.iter().map(|item| {
                            match item {
                                PhpArrayItem::KeyValue(k, v) => format!(
                                    "{}={}",
                                    urlencoding::encode(k),
                                    urlencoding::encode(&v.as_string())
                                ),
                                PhpArrayItem::Value(v) => urlencoding::encode(&v.as_string()).to_string(),
                            }
                        }).collect();
                        Ok(PhpValue::String(pairs.join(sep)))
                    }
                    _ => Ok(PhpValue::String(String::new())),
                }
            }
            "urlencode" => {
                let s = args.first().map_or(String::new(), |a| a.as_string());
                Ok(PhpValue::String(urlencoding::encode(&s).to_string()))
            }
            "urldecode" => {
                let s = args.first().map_or(String::new(), |a| a.as_string());
                Ok(PhpValue::String(
                    urlencoding::decode(&s).unwrap_or_default().to_string(),
                ))
            }
            "rawurlencode" => {
                let s = args.first().map_or(String::new(), |a| a.as_string());
                Ok(PhpValue::String(urlencoding::encode(&s).to_string()))
            }
            "rawurldecode" => {
                let s = args.first().map_or(String::new(), |a| a.as_string());
                Ok(PhpValue::String(
                    urlencoding::decode(&s).unwrap_or_default().to_string(),
                ))
            }
            "ucfirst" => {
                let s = args.first().map_or(String::new(), |a| a.as_string());
                let mut c = s.chars();
                Ok(PhpValue::String(match c.next() {
                    None => String::new(),
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                }))
            }
            "lcfirst" => {
                let s = args.first().map_or(String::new(), |a| a.as_string());
                let mut c = s.chars();
                Ok(PhpValue::String(match c.next() {
                    None => String::new(),
                    Some(f) => f.to_lowercase().collect::<String>() + c.as_str(),
                }))
            }
            "str_repeat" => {
                let s = args.first().map_or(String::new(), |a| a.as_string());
                let n = args.get(1).map_or(0, |a| a.as_int().max(0)) as usize;
                Ok(PhpValue::String(s.repeat(n)))
            }
            "str_pad" => {
                let input = args.first().map_or(String::new(), |a| a.as_string());
                let length = args.get(1).map_or(0, |a| a.as_int()) as usize;
                let pad = args.get(2).map_or(" ".to_string(), |a| a.as_string());
                if input.len() >= length || pad.is_empty() {
                    Ok(PhpValue::String(input))
                } else {
                    let needed = length - input.len();
                    let padding: String = pad.chars().cycle().take(needed).collect();
                    Ok(PhpValue::String(format!("{}{}", input, padding)))
                }
            }
            "md5" => {
                let s = args.first().map_or(String::new(), |a| a.as_string());
                Ok(PhpValue::String(format!("{:x}", md5_hash(s.as_bytes()))))
            }
            "sha1" => {
                let s = args.first().map_or(String::new(), |a| a.as_string());
                let mut hasher = Sha1::new();
                hasher.update(s.as_bytes());
                let result = hasher.finalize();
                Ok(PhpValue::String(format!("{:x}", result)))
            }
            "hash" | "hash_hmac" => {
                // hash('sha1', $data) or hash('md5', $data) — simplified
                let algo = args.first().map_or(String::new(), |a| a.as_string()).to_lowercase();
                let data = args.get(1).map_or(String::new(), |a| a.as_string());
                match algo.as_str() {
                    "sha1" => {
                        let mut hasher = Sha1::new();
                        hasher.update(data.as_bytes());
                        Ok(PhpValue::String(format!("{:x}", hasher.finalize())))
                    }
                    "md5" => Ok(PhpValue::String(format!("{:x}", md5_hash(data.as_bytes())))),
                    _ => {
                        self.log_error(&format!("hash: unsupported algorithm '{}'", algo));
                        Ok(PhpValue::String(String::new()))
                    }
                }
            }
            "number_format" => {
                let num = args.first().map_or(0.0, |a| a.as_float());
                let decimals = args.get(1).map_or(0, |a| a.as_int()) as usize;
                Ok(PhpValue::String(format!("{:.prec$}", num, prec = decimals)))
            }
            "intval" | "int" => Ok(PhpValue::Int(args.first().map_or(0, |a| a.as_int()))),
            "floatval" | "doubleval" => {
                Ok(PhpValue::Float(args.first().map_or(0.0, |a| a.as_float())))
            }
            "strval" => Ok(PhpValue::String(
                args.first().map_or(String::new(), |a| a.as_string()),
            )),
            "boolval" => Ok(PhpValue::Bool(
                args.first().map_or(false, |a| a.is_truthy()),
            )),

            // ── Array functions ──────────────────────────────────────────
            "count" | "sizeof" => Ok(PhpValue::Int(args.first().map_or(0, |a| a.count()) as i64)),
            "array_is_list" => {
                // Returns true if the array is a list (sequential 0-based integer keys)
                match args.first() {
                    Some(PhpValue::Array(items)) => {
                        let is_list = items.iter().enumerate().all(|(i, item)| {
                            match item {
                                PhpArrayItem::Value(_) => true,
                                PhpArrayItem::KeyValue(k, _) => k == &i.to_string(),
                            }
                        });
                        Ok(PhpValue::Bool(is_list))
                    }
                    _ => Ok(PhpValue::Bool(false)),
                }
            }
            "array_keys" => {
                if let Some(PhpValue::Array(items)) = args.first() {
                    let keys: Vec<PhpArrayItem> = items
                        .iter()
                        .enumerate()
                        .map(|(i, item)| {
                            let key = match item {
                                PhpArrayItem::KeyValue(k, _) => k.clone(),
                                PhpArrayItem::Value(_) => i.to_string(),
                            };
                            PhpArrayItem::Value(PhpValue::String(key))
                        })
                        .collect();
                    Ok(PhpValue::Array(keys))
                } else {
                    Ok(PhpValue::Array(vec![]))
                }
            }
            "array_values" => {
                if let Some(PhpValue::Array(items)) = args.first() {
                    let values: Vec<PhpArrayItem> = items
                        .iter()
                        .map(|item| {
                            let val = match item {
                                PhpArrayItem::KeyValue(_, v) => v.clone(),
                                PhpArrayItem::Value(v) => v.clone(),
                            };
                            PhpArrayItem::Value(val)
                        })
                        .collect();
                    Ok(PhpValue::Array(values))
                } else {
                    Ok(PhpValue::Array(vec![]))
                }
            }
            "in_array" => {
                let needle = args.first().unwrap_or(&PhpValue::Null);
                if let Some(PhpValue::Array(items)) = args.get(1) {
                    let found = items.iter().any(|item| {
                        let val = match item {
                            PhpArrayItem::KeyValue(_, v) => v,
                            PhpArrayItem::Value(v) => v,
                        };
                        needle.loose_eq(val)
                    });
                    Ok(PhpValue::Bool(found))
                } else {
                    Ok(PhpValue::Bool(false))
                }
            }
            "array_key_exists" => {
                let key = args.first().map_or(String::new(), |a| a.as_string());
                if let Some(PhpValue::Array(items)) = args.get(1) {
                    let found = items
                        .iter()
                        .any(|item| matches!(item, PhpArrayItem::KeyValue(k, _) if k == &key));
                    Ok(PhpValue::Bool(found))
                } else {
                    Ok(PhpValue::Bool(false))
                }
            }
            "array_merge" => {
                let mut merged = Vec::new();
                for arg in &args {
                    if let PhpValue::Array(items) = arg {
                        merged.extend(items.clone());
                    }
                }
                Ok(PhpValue::Array(merged))
            }
            "array_push" => {
                // Note: in real PHP this modifies by reference; here we just return
                if let Some(PhpValue::Array(mut items)) = args.first().cloned() {
                    for arg in args.iter().skip(1) {
                        items.push(PhpArrayItem::Value(arg.clone()));
                    }
                    Ok(PhpValue::Int(items.len() as i64))
                } else {
                    Ok(PhpValue::Bool(false))
                }
            }
            "array_pop" => {
                // Similar limitation — returns the popped value
                if let Some(PhpValue::Array(items)) = args.first() {
                    if let Some(last) = items.last() {
                        let val = match last {
                            PhpArrayItem::KeyValue(_, v) => v.clone(),
                            PhpArrayItem::Value(v) => v.clone(),
                        };
                        Ok(val)
                    } else {
                        Ok(PhpValue::Null)
                    }
                } else {
                    Ok(PhpValue::Null)
                }
            }
            "array_map" => {
                // array_map(null, $arr) just returns $arr; callback not supported yet
                if let Some(PhpValue::Array(items)) = args.get(1) {
                    Ok(PhpValue::Array(items.clone()))
                } else {
                    Ok(PhpValue::Array(vec![]))
                }
            }
            "array_slice" => {
                if let Some(PhpValue::Array(items)) = args.first() {
                    let offset = args.get(1).map_or(0, |a| a.as_int().max(0)) as usize;
                    let length = args.get(2).map(|a| a.as_int() as usize);
                    let end = length.map_or(items.len(), |l| (offset + l).min(items.len()));
                    let offset = offset.min(items.len());
                    Ok(PhpValue::Array(items[offset..end].to_vec()))
                } else {
                    Ok(PhpValue::Array(vec![]))
                }
            }
            "array_reverse" => {
                if let Some(PhpValue::Array(items)) = args.first() {
                    let mut reversed = items.clone();
                    reversed.reverse();
                    Ok(PhpValue::Array(reversed))
                } else {
                    Ok(PhpValue::Array(vec![]))
                }
            }
            "array_unique" => {
                if let Some(PhpValue::Array(items)) = args.first() {
                    let mut seen = HashSet::new();
                    let mut unique = Vec::new();
                    for item in items {
                        let key = match item {
                            PhpArrayItem::KeyValue(_, v) => v.as_string(),
                            PhpArrayItem::Value(v) => v.as_string(),
                        };
                        if seen.insert(key) {
                            unique.push(item.clone());
                        }
                    }
                    Ok(PhpValue::Array(unique))
                } else {
                    Ok(PhpValue::Array(vec![]))
                }
            }
            "sort" | "rsort" | "asort" | "arsort" | "ksort" | "krsort" => {
                // Returns true; actual sort is a no-op since we can't mutate by reference
                Ok(PhpValue::Bool(true))
            }
            "range" => {
                let start = args.first().map_or(0, |a| a.as_int());
                let end = args.get(1).map_or(0, |a| a.as_int());
                let step = args.get(2).map_or(1, |a| a.as_int().max(1));
                let mut items = Vec::new();
                let mut i = start;
                if start <= end {
                    while i <= end {
                        items.push(PhpArrayItem::Value(PhpValue::Int(i)));
                        i += step;
                    }
                } else {
                    while i >= end {
                        items.push(PhpArrayItem::Value(PhpValue::Int(i)));
                        i -= step;
                    }
                }
                Ok(PhpValue::Array(items))
            }
            "compact" => {
                let mut items = Vec::new();
                for arg in &args {
                    let name = arg.as_string();
                    let value = self.variables.get(&name).cloned().unwrap_or(PhpValue::Null);
                    items.push(PhpArrayItem::KeyValue(name, value));
                }
                Ok(PhpValue::Array(items))
            }
            "extract" => {
                if let Some(PhpValue::Array(items)) = args.first() {
                    for item in items {
                        if let PhpArrayItem::KeyValue(k, v) = item {
                            self.variables.insert(k.clone(), v.clone());
                        }
                    }
                }
                Ok(PhpValue::Null)
            }

            // ── JSON ────────────────────────────────────────────────────
            "json_encode" => {
                let val = args.first().unwrap_or(&PhpValue::Null);
                let flags = args.get(1).map_or(0, |a| a.as_int());
                let mut s = php_value_to_json(val);
                // JSON_UNESCAPED_SLASHES (64) — don't escape forward slashes
                if flags & 64 != 0 {
                    s = s.replace("\\/", "/");
                }
                // JSON_PRETTY_PRINT (128) — not implemented, just pass through
                Ok(PhpValue::String(s))
            }
            "json_decode" => {
                let s = args.first().map_or(String::new(), |a| a.as_string());
                let assoc = args.get(1).map_or(false, |a| a.is_truthy());
                match serde_json::from_str::<serde_json::Value>(&s) {
                    Ok(json) => Ok(json_to_php_value(&json, assoc)),
                    Err(_) => Ok(PhpValue::Null),
                }
            }

            // ── Date/time ───────────────────────────────────────────────
            "time" => Ok(PhpValue::Int(chrono::Utc::now().timestamp())),
            "microtime" => {
                let as_float = args.first().map_or(false, |a| a.is_truthy());
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
                if as_float {
                    Ok(PhpValue::Float(now.as_secs_f64()))
                } else {
                    let secs = now.as_secs();
                    let micros = now.subsec_micros();
                    Ok(PhpValue::String(format!("0.{:06} {}", micros, secs)))
                }
            }
            "strtotime" => {
                // Simplified: only handles "now" and timestamps
                let s = args.first().map_or(String::new(), |a| a.as_string());
                if s == "now" || s.is_empty() {
                    Ok(PhpValue::Int(chrono::Utc::now().timestamp()))
                } else {
                    // Try to parse as ISO date
                    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S") {
                        Ok(PhpValue::Int(dt.and_utc().timestamp()))
                    } else if let Ok(d) = chrono::NaiveDate::parse_from_str(&s, "%Y-%m-%d") {
                        Ok(PhpValue::Int(
                            d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp(),
                        ))
                    } else {
                        Ok(PhpValue::Bool(false))
                    }
                }
            }

            // ── Filesystem ──────────────────────────────────────────────
            "file_exists" | "is_readable" | "is_writable" => {
                let path_str = args.first().map_or(String::new(), |a| a.as_string());
                let result = match self.resolve_local_path(&path_str) {
                    Ok(path) => path.exists(),
                    Err(_) => false,
                };
                Ok(PhpValue::Bool(result))
            }
            "is_file" => {
                let path_str = args.first().map_or(String::new(), |a| a.as_string());
                let result = match self.resolve_local_path(&path_str) {
                    Ok(path) => path.is_file(),
                    Err(_) => false,
                };
                Ok(PhpValue::Bool(result))
            }
            "is_dir" => {
                let path_str = args.first().map_or(String::new(), |a| a.as_string());
                let result = match self.resolve_local_path(&path_str) {
                    Ok(path) => path.is_dir(),
                    Err(_) => false,
                };
                Ok(PhpValue::Bool(result))
            }
            "filemtime" => {
                let path_str = args.first().map_or(String::new(), |a| a.as_string());
                match self.resolve_local_path(&path_str) {
                    Ok(path) => {
                        match std::fs::metadata(&path) {
                            Ok(meta) => {
                                match meta.modified() {
                                    Ok(time) => {
                                        let epoch = time.duration_since(std::time::UNIX_EPOCH)
                                            .map_or(0, |d| d.as_secs() as i64);
                                        Ok(PhpValue::Int(epoch))
                                    }
                                    Err(_) => Ok(PhpValue::Bool(false)),
                                }
                            }
                            Err(_) => Ok(PhpValue::Bool(false)),
                        }
                    }
                    Err(_) => Ok(PhpValue::Bool(false)),
                }
            }
            "mkdir" => {
                let path_str = args.first().map_or(String::new(), |a| a.as_string());
                let _mode = args.get(1).map_or(0o777, |a| a.as_int() as u32);
                let recursive = args.get(2).map_or(false, |a| a.is_truthy());
                let result = if recursive {
                    std::fs::create_dir_all(&path_str).is_ok()
                } else {
                    std::fs::create_dir(&path_str).is_ok()
                };
                Ok(PhpValue::Bool(result))
            }
            "dirname" => {
                let path_str = args.first().map_or(String::new(), |a| a.as_string());
                let levels = args.get(1).map_or(1, |a| a.as_int().max(1)) as usize;
                let mut p = Path::new(&path_str).to_path_buf();
                for _ in 0..levels {
                    if let Some(parent) = p.parent() {
                        p = parent.to_path_buf();
                    } else {
                        break;
                    }
                }
                Ok(PhpValue::String(p.to_string_lossy().to_string()))
            }
            "basename" => {
                let path_str = args.first().map_or(String::new(), |a| a.as_string());
                Ok(PhpValue::String(
                    Path::new(&path_str)
                        .file_name()
                        .map_or(String::new(), |n| n.to_string_lossy().to_string()),
                ))
            }
            "pathinfo" => {
                let path_str = args.first().map_or(String::new(), |a| a.as_string());
                let flag = args.get(1).map(|a| a.as_int());
                let p = Path::new(&path_str);
                match flag {
                    Some(1) => {
                        Ok(PhpValue::String(p.parent().map_or(String::new(), |pp| {
                            pp.to_string_lossy().to_string()
                        })))
                    }
                    Some(2) => {
                        Ok(PhpValue::String(p.file_name().map_or(String::new(), |n| {
                            n.to_string_lossy().to_string()
                        })))
                    }
                    Some(4) => {
                        Ok(PhpValue::String(p.extension().map_or(String::new(), |e| {
                            e.to_string_lossy().to_string()
                        })))
                    }
                    Some(8) => {
                        Ok(PhpValue::String(p.file_stem().map_or(String::new(), |s| {
                            s.to_string_lossy().to_string()
                        })))
                    }
                    _ => {
                        let items =
                            vec![
                                PhpArrayItem::KeyValue(
                                    "dirname".to_string(),
                                    PhpValue::String(p.parent().map_or(String::new(), |pp| {
                                        pp.to_string_lossy().to_string()
                                    })),
                                ),
                                PhpArrayItem::KeyValue(
                                    "basename".to_string(),
                                    PhpValue::String(p.file_name().map_or(String::new(), |n| {
                                        n.to_string_lossy().to_string()
                                    })),
                                ),
                                PhpArrayItem::KeyValue(
                                    "extension".to_string(),
                                    PhpValue::String(p.extension().map_or(String::new(), |e| {
                                        e.to_string_lossy().to_string()
                                    })),
                                ),
                                PhpArrayItem::KeyValue(
                                    "filename".to_string(),
                                    PhpValue::String(p.file_stem().map_or(String::new(), |s| {
                                        s.to_string_lossy().to_string()
                                    })),
                                ),
                            ];
                        Ok(PhpValue::Array(items))
                    }
                }
            }
            "realpath" => {
                let path_str = args.first().map_or(String::new(), |a| a.as_string());
                match self.resolve_local_path(&path_str) {
                    Ok(path) => Ok(PhpValue::String(path.to_string_lossy().to_string())),
                    Err(_) => Ok(PhpValue::Bool(false)),
                }
            }
            "parse_url" => {
                let url_str = args.first().map_or(String::new(), |a| a.as_string());
                let component = args.get(1).map(|a| a.as_int());

                // Simple URL parser: [scheme://][user[:pass]@]host[:port][/path][?query][#fragment]
                let mut scheme = String::new();
                let mut host = String::new();
                let mut port = String::new();
                let mut user = String::new();
                let mut pass = String::new();
                let mut query = String::new();
                let mut fragment = String::new();

                let mut rest = url_str.as_str();

                // Fragment
                if let Some(hash) = rest.find('#') {
                    fragment = rest[hash + 1..].to_string();
                    rest = &rest[..hash];
                }
                // Query
                if let Some(q) = rest.find('?') {
                    query = rest[q + 1..].to_string();
                    rest = &rest[..q];
                }
                // Scheme + path
                let path;
                if let Some(colon) = rest.find("://") {
                    scheme = rest[..colon].to_string();
                    rest = &rest[colon + 3..];
                    // Authority (user:pass@host:port)
                    let (authority, path_part) = if let Some(slash) = rest.find('/') {
                        (&rest[..slash], &rest[slash..])
                    } else {
                        (rest, "")
                    };
                    path = path_part.to_string();
                    // user:pass@
                    let host_part = if let Some(at) = authority.find('@') {
                        let userinfo = &authority[..at];
                        if let Some(colon) = userinfo.find(':') {
                            user = userinfo[..colon].to_string();
                            pass = userinfo[colon + 1..].to_string();
                        } else {
                            user = userinfo.to_string();
                        }
                        &authority[at + 1..]
                    } else {
                        authority
                    };
                    // host:port
                    if let Some(colon) = host_part.rfind(':') {
                        host = host_part[..colon].to_string();
                        port = host_part[colon + 1..].to_string();
                    } else {
                        host = host_part.to_string();
                    }
                } else {
                    // No scheme — treat as path (like "/foo/bar" or "foo/bar")
                    path = rest.to_string();
                }

                match component {
                    Some(0) => Ok(PhpValue::String(scheme)), // PHP_URL_SCHEME
                    Some(1) => Ok(PhpValue::String(host)),   // PHP_URL_HOST
                    Some(2) => Ok(if port.is_empty() {
                        PhpValue::Null
                    } else {
                        // PHP_URL_PORT
                        PhpValue::Int(port.parse::<i64>().unwrap_or(0))
                    }),
                    Some(3) => Ok(PhpValue::String(user)), // PHP_URL_USER
                    Some(4) => Ok(PhpValue::String(pass)), // PHP_URL_PASS
                    Some(5) => Ok(PhpValue::String(path)), // PHP_URL_PATH
                    Some(6) => Ok(PhpValue::String(query)), // PHP_URL_QUERY
                    Some(7) => Ok(PhpValue::String(fragment)), // PHP_URL_FRAGMENT
                    _ => {
                        // Return full array
                        let mut items = Vec::new();
                        if !scheme.is_empty() {
                            items.push(PhpArrayItem::KeyValue(
                                "scheme".to_string(),
                                PhpValue::String(scheme),
                            ));
                        }
                        if !host.is_empty() {
                            items.push(PhpArrayItem::KeyValue(
                                "host".to_string(),
                                PhpValue::String(host),
                            ));
                        }
                        if !port.is_empty() {
                            items.push(PhpArrayItem::KeyValue(
                                "port".to_string(),
                                PhpValue::Int(port.parse::<i64>().unwrap_or(0)),
                            ));
                        }
                        if !user.is_empty() {
                            items.push(PhpArrayItem::KeyValue(
                                "user".to_string(),
                                PhpValue::String(user),
                            ));
                        }
                        if !pass.is_empty() {
                            items.push(PhpArrayItem::KeyValue(
                                "pass".to_string(),
                                PhpValue::String(pass),
                            ));
                        }
                        if !path.is_empty() {
                            items.push(PhpArrayItem::KeyValue(
                                "path".to_string(),
                                PhpValue::String(path),
                            ));
                        }
                        if !query.is_empty() {
                            items.push(PhpArrayItem::KeyValue(
                                "query".to_string(),
                                PhpValue::String(query),
                            ));
                        }
                        if !fragment.is_empty() {
                            items.push(PhpArrayItem::KeyValue(
                                "fragment".to_string(),
                                PhpValue::String(fragment),
                            ));
                        }
                        Ok(PhpValue::Array(items))
                    }
                }
            }
            "file_put_contents" => {
                let path_str = args.first().map_or(String::new(), |a| a.as_string());
                let content = args.get(1).map_or(String::new(), |a| a.as_string());
                let path = match self.resolve_local_path(&path_str) {
                    Ok(p) => p,
                    Err(e) => {
                        self.log_error(&format!("file_put_contents({}): {}", path_str, e));
                        return Ok(PhpValue::Bool(false));
                    }
                };
                // 3rd arg: flags — FILE_APPEND = 8, LOCK_EX = 2
                let flags = args.get(2).map_or(0, |a| a.as_int());
                let file_append = (flags & 8) != 0;
                let len = content.len() as i64;
                if file_append {
                    let p = path.clone();
                    let c = content.clone();
                    let result = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                        use std::io::Write;
                        let mut file = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&p)?;
                        file.write_all(c.as_bytes())
                    })
                    .await;
                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            self.log_error(&format!("file_put_contents({}): {}", path_str, e));
                            return Ok(PhpValue::Bool(false));
                        }
                        Err(e) => {
                            self.log_error(&format!("file_put_contents({}): {}", path_str, e));
                            return Ok(PhpValue::Bool(false));
                        }
                    }
                } else {
                    if let Err(e) = fs::write(&path, &content).await {
                        self.log_error(&format!("file_put_contents({}): {}", path_str, e));
                        return Ok(PhpValue::Bool(false));
                    }
                }
                Ok(PhpValue::Int(len))
            }
            "filesize" => {
                let path_str = args.first().map_or(String::new(), |a| a.as_string());
                match self.resolve_local_path(&path_str) {
                    Ok(path) => match std::fs::metadata(&path) {
                        Ok(meta) => Ok(PhpValue::Int(meta.len() as i64)),
                        Err(_) => Ok(PhpValue::Bool(false)),
                    },
                    Err(_) => Ok(PhpValue::Bool(false)),
                }
            }
            "glob" => {
                // Simplified glob using std
                let pattern = args.first().map_or(String::new(), |a| a.as_string());
                let root = self.root_dir.clone().unwrap_or_else(|| PathBuf::from("."));
                let full_pattern = if pattern.starts_with('/') {
                    root.join(pattern.trim_start_matches('/'))
                } else {
                    self.current_template_path
                        .as_ref()
                        .and_then(|p| p.parent())
                        .unwrap_or(&root)
                        .join(&pattern)
                };
                let mut items = Vec::new();
                if let Ok(entries) = ::std::fs::read_dir(full_pattern.parent().unwrap_or(&root)) {
                    for entry in entries.flatten() {
                        items.push(PhpArrayItem::Value(PhpValue::String(
                            entry.path().to_string_lossy().to_string(),
                        )));
                    }
                }
                Ok(PhpValue::Array(items))
            }

            // ── Math ────────────────────────────────────────────────────
            "abs" => Ok(PhpValue::Float(
                args.first().map_or(0.0, |a| a.as_float().abs()),
            )),
            "ceil" => Ok(PhpValue::Float(
                args.first().map_or(0.0, |a| a.as_float().ceil()),
            )),
            "floor" => Ok(PhpValue::Float(
                args.first().map_or(0.0, |a| a.as_float().floor()),
            )),
            "round" => {
                let val = args.first().map_or(0.0, |a| a.as_float());
                let precision = args.get(1).map_or(0, |a| a.as_int()) as i32;
                let factor = 10f64.powi(precision);
                Ok(PhpValue::Float((val * factor).round() / factor))
            }
            "max" => {
                if args.len() == 1 {
                    if let Some(PhpValue::Array(items)) = args.first() {
                        let max = items
                            .iter()
                            .map(|i| match i {
                                PhpArrayItem::KeyValue(_, v) => v.as_float(),
                                PhpArrayItem::Value(v) => v.as_float(),
                            })
                            .fold(f64::NEG_INFINITY, f64::max);
                        return Ok(PhpValue::Float(max));
                    }
                }
                let max = args
                    .iter()
                    .map(|a| a.as_float())
                    .fold(f64::NEG_INFINITY, f64::max);
                Ok(PhpValue::Float(max))
            }
            "min" => {
                if args.len() == 1 {
                    if let Some(PhpValue::Array(items)) = args.first() {
                        let min = items
                            .iter()
                            .map(|i| match i {
                                PhpArrayItem::KeyValue(_, v) => v.as_float(),
                                PhpArrayItem::Value(v) => v.as_float(),
                            })
                            .fold(f64::INFINITY, f64::min);
                        return Ok(PhpValue::Float(min));
                    }
                }
                let min = args
                    .iter()
                    .map(|a| a.as_float())
                    .fold(f64::INFINITY, f64::min);
                Ok(PhpValue::Float(min))
            }
            "rand" | "mt_rand" => {
                let min = args.first().map_or(0, |a| a.as_int());
                let max = args.get(1).map_or(i32::MAX as i64, |a| a.as_int());
                // Simple LCG random — not cryptographic
                let seed = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .subsec_nanos() as i64;
                let range = (max - min + 1).max(1);
                Ok(PhpValue::Int(min + (seed.abs() % range)))
            }

            // ── Regex ───────────────────────────────────────────────────
            // preg_match / preg_match_all are handled earlier in evaluate_function_call
            // (they need raw args_node to write captures into the 3rd argument variable)
            "preg_quote" => {
                let s = args.first().map_or(String::new(), |a| a.as_string());
                let delimiter = args.get(1).map_or(String::new(), |a| a.as_string());
                let mut out = String::with_capacity(s.len() * 2);
                for ch in s.chars() {
                    if ".\\+*?[^]$(){}=!<>|:-#".contains(ch)
                        || (!delimiter.is_empty() && delimiter.contains(ch))
                    {
                        out.push('\\');
                    }
                    out.push(ch);
                }
                Ok(PhpValue::String(out))
            }
            "preg_replace" => {
                let pattern = args.first().map_or(String::new(), |a| a.as_string());
                let replacement = args.get(1).map_or(String::new(), |a| a.as_string());
                let subject = args.get(2).map_or(String::new(), |a| a.as_string());
                let re_str = build_regex_with_flags(&pattern);
                match Regex::new(&re_str) {
                    Ok(re) => Ok(PhpValue::String(
                        re.replace_all(&subject, replacement.as_str()).to_string(),
                    )),
                    Err(_) => Ok(PhpValue::String(subject)),
                }
            }
            "preg_split" => {
                let pattern = args.first().map_or(String::new(), |a| a.as_string());
                let subject = args.get(1).map_or(String::new(), |a| a.as_string());
                let re_str = build_regex_with_flags(&pattern);
                match Regex::new(&re_str) {
                    Ok(re) => {
                        let parts: Vec<PhpArrayItem> = re
                            .split(&subject)
                            .map(|s| PhpArrayItem::Value(PhpValue::String(s.to_string())))
                            .collect();
                        Ok(PhpValue::Array(parts))
                    }
                    Err(_) => Ok(PhpValue::Array(vec![PhpArrayItem::Value(
                        PhpValue::String(subject),
                    )])),
                }
            }
            "strip_tags" => {
                let s = args.first().map_or(String::new(), |a| a.as_string());
                // Remove all HTML tags using a simple regex
                let out = Regex::new(r"<[^>]+>")
                    .map(|re| re.replace_all(&s, "").to_string())
                    .unwrap_or(s);
                Ok(PhpValue::String(out))
            }

            // ── Misc ────────────────────────────────────────────────────
            "define" => {
                let name = args.first().map_or(String::new(), |a| a.as_string());
                let value = args.get(1).cloned().unwrap_or(PhpValue::Null);
                debug!("[{}] PHP define: {} = {:?}", self.request_domain, name, value);
                self.constants.insert(name, value);
                Ok(PhpValue::Bool(true))
            }
            "defined" => {
                let name = args.first().map_or(String::new(), |a| a.as_string());
                Ok(PhpValue::Bool(self.constants.contains_key(&name)))
            }
            "constant" => {
                let name = args.first().map_or(String::new(), |a| a.as_string());
                Ok(self.constants.get(&name).cloned().unwrap_or(PhpValue::Null))
            }
            "function_exists" => {
                let name = args.first().map_or(String::new(), |a| a.as_string());
                let is_builtin = matches!(
                    name.as_str(),
                    "echo"
                        | "print"
                        | "isset"
                        | "empty"
                        | "exit"
                        | "die"
                        | "strlen"
                        | "strtolower"
                        | "strtoupper"
                        | "trim"
                        | "substr"
                        | "str_replace"
                        | "str_contains"
                        | "explode"
                        | "implode"
                        | "count"
                        | "array_keys"
                        | "array_values"
                        | "in_array"
                        | "json_encode"
                        | "json_decode"
                        | "date"
                        | "time"
                        | "file_exists"
                        | "is_file"
                        | "is_dir"
                        | "is_readable"
                        | "readfile"
                        | "header"
                        | "http_response_code"
                        | "var_dump"
                        | "print_r"
                        | "parse_url"
                        | "basename"
                        | "dirname"
                        | "pathinfo"
                        | "realpath"
                        | "rtrim"
                        | "ltrim"
                        | "file_get_contents"
                        | "filesize"
                        | "error_log"
                        | "trigger_error"
                        | "preg_match"
                        | "strpos"
                        | "stripos"
                        | "is_array"
                        | "is_string"
                        | "is_numeric"
                        | "is_int"
                        | "is_bool"
                        | "is_scalar"
                        | "is_null"
                        | "is_float"
                        | "gettype"
                        | "file_put_contents"
                        | "http_request"
                        | "ruph_session_start"
                        | "ruph_session_destroy"
                        | "ruph_session_id"
                );
                Ok(PhpValue::Bool(
                    is_builtin || self.user_functions.contains_key(&name),
                ))
            }
            "print_r" => {
                let val = args.first().unwrap_or(&PhpValue::Null);
                let return_output = args.get(1).map_or(false, |a| a.is_truthy());
                let s = val.dump();
                if return_output {
                    Ok(PhpValue::String(s))
                } else {
                    self.side_effect_output = Some(s);
                    Ok(PhpValue::Bool(true))
                }
            }
            "error_log" => {
                let msg = args.first().map_or(String::new(), |a| a.as_string());
                warn!("[{}] PHP error_log: {}", self.request_domain, msg);
                self.log_error(&msg);
                Ok(PhpValue::Bool(true))
            }
            "trigger_error" => {
                let msg = args.first().map_or(String::new(), |a| a.as_string());
                let level = args.get(1).map_or(256, |a| a.as_int()); // E_USER_ERROR = 256
                let level_str = match level {
                    256 => "E_USER_ERROR",
                    512 => "E_USER_WARNING",
                    1024 => "E_USER_NOTICE",
                    16384 => "E_USER_DEPRECATED",
                    _ => "E_USER_NOTICE",
                };
                self.log_error(&format!("{}: {}", level_str, msg));
                Ok(PhpValue::Bool(true))
            }
            "setcookie" => {
                // Set-Cookie header
                let name = args.first().map_or(String::new(), |a| a.as_string());
                let value = args.get(1).map_or(String::new(), |a| a.as_string());
                let cookie = format!("{}={}", name, value);
                self.add_response_header("set-cookie", cookie);
                Ok(PhpValue::Bool(true))
            }
            "ruph_session_start" => self.ruph_session_start(&args).await,
            "ruph_session_destroy" => self.ruph_session_destroy().await,
            "ruph_session_id" => Ok(PhpValue::String(
                self.session_id.clone().unwrap_or_default(),
            )),
            // unset() is handled specially before arg evaluation (see above)
            // This fallback handles edge cases where it arrives as a regular call
            "unset" => Ok(PhpValue::Null),
            "sleep" => {
                let secs = args.first().map_or(0, |a| a.as_int() as u64);
                tokio::time::sleep(std::time::Duration::from_secs(secs.min(30))).await;
                Ok(PhpValue::Int(0))
            }
            "usleep" => {
                let micros = args.first().map_or(0, |a| a.as_int() as u64);
                // Cap at 200ms to prevent stalls; real non-blocking loops use small values
                tokio::time::sleep(std::time::Duration::from_micros(micros.min(200_000))).await;
                Ok(PhpValue::Int(0))
            }
            "flush" | "ob_flush" | "ob_end_flush" => {
                if let Some(tx) = self.flush_tx.clone() {
                    // On first flush: commit response headers to the waiting caller
                    if let Some(htx) = self.headers_tx.take() {
                        debug!("[{}] PHP flush: committing headers (streaming mode)", self.request_domain);
                        let _ = htx.send((self.response_headers.clone(), self.response_status));
                    }
                    // Send accumulated echo output as a body chunk
                    let chunk = std::mem::take(&mut self.streaming_buf);
                    if !chunk.is_empty() {
                        debug!("[{}] PHP flush: sending {} bytes", self.request_domain, chunk.len());
                        let _ = tx.send(Ok(Bytes::from(chunk.into_bytes()))).await;
                    }
                } else {
                    debug!("[{}] PHP flush: not in streaming mode, buf_len={}", self.request_domain, self.streaming_buf.len());
                }
                Ok(PhpValue::Null)
            }
            "getenv" => {
                let key = args.first().map_or(String::new(), |a| a.as_string());
                if key.is_empty() {
                    Ok(PhpValue::Bool(false))
                } else {
                    Ok(std::env::var(&key)
                        .map(PhpValue::String)
                        .unwrap_or(PhpValue::Bool(false)))
                }
            }
            "stream_context_create" => {
                // Return a dummy resource (context options ignored; TLS is handled by connect_tls)
                Ok(PhpValue::Resource(0))
            }
            "stream_socket_client" => {
                // stream_socket_client("ssl://host:port", &$errno, &$errstr, $timeout, $flags, $context)
                let addr_str = args.first().map_or(String::new(), |a| a.as_string());
                debug!("[{}] PHP stream_socket_client: {}", self.request_domain, addr_str);
                let is_ssl = addr_str.starts_with("ssl://");
                let host_port = addr_str
                    .trim_start_matches("ssl://")
                    .trim_start_matches("tcp://");
                let (host, port_str) = if let Some(colon) = host_port.rfind(':') {
                    (
                        host_port[..colon].to_string(),
                        host_port[colon + 1..].to_string(),
                    )
                } else {
                    (
                        host_port.to_string(),
                        if is_ssl {
                            "443".to_string()
                        } else {
                            "80".to_string()
                        },
                    )
                };
                let port: u16 = port_str.parse().unwrap_or(if is_ssl { 443 } else { 80 });
                let timeout_secs = args.get(3).map_or(30u64, |a| a.as_int().max(1) as u64);

                let connect_result = tokio::time::timeout(
                    std::time::Duration::from_secs(timeout_secs),
                    self.connect_tls(&host, port),
                )
                .await;

                match connect_result {
                    Ok(Ok(tls_stream)) => {
                        let id = self.next_stream_id;
                        self.next_stream_id += 1;
                        debug!("[{}] PHP stream_socket_client: connected, handle={}", self.request_domain, id);
                        self.streams.insert(
                            id,
                            PhpTlsStream {
                                stream: tls_stream,
                                eof: false,
                                read_buffer: Vec::new(),
                                write_buffer: Vec::new(),
                                streaming_mode: false,
                            },
                        );
                        Ok(PhpValue::Resource(id))
                    }
                    Ok(Err(e)) => {
                        self.log_error(&format!("stream_socket_client failed: {}", e));
                        Ok(PhpValue::Bool(false))
                    }
                    Err(_) => {
                        self.log_error("stream_socket_client: connection timed out");
                        Ok(PhpValue::Bool(false))
                    }
                }
            }
            "stream_set_blocking" => {
                // stream_set_blocking($stream, $blocking)
                // We use this to control streaming mode:
                // blocking=false enables real-time streaming (no write buffering)
                // blocking=true enables buffering for efficiency
                let handle = if let Some(PhpValue::Resource(id)) = args.first() {
                    *id
                } else {
                    0
                };
                let blocking = args.get(1).map_or(true, |a| a.is_truthy());

                if handle != 0 {
                    if let Some(php_stream) = self.streams.get_mut(&handle) {
                        // Non-blocking = streaming mode (immediate flush)
                        php_stream.streaming_mode = !blocking;
                        debug!("[{}] PHP stream_set_blocking: handle={}, streaming_mode={}",
                               self.request_domain, handle, !blocking);
                    }
                }
                Ok(PhpValue::Bool(true))
            }
            "fwrite" => {
                let handle = if let Some(PhpValue::Resource(id)) = args.first() {
                    *id
                } else {
                    0
                };
                let data = args.get(1).map_or(String::new(), |a| a.as_string());
                let length = args.get(2).map_or(data.len(), |a| a.as_int().max(0) as usize);

                if handle == 0 {
                    return Ok(PhpValue::Bool(false));
                }

                if let Some(mut php_stream) = self.streams.remove(&handle) {
                    let bytes = if length < data.len() {
                        data[..length].to_string().into_bytes()
                    } else {
                        data.into_bytes()
                    };
                    let len = bytes.len() as i64;

                    // In streaming mode, always write immediately
                    if php_stream.streaming_mode {
                        // Flush any buffered data first
                        let to_write = if !php_stream.write_buffer.is_empty() {
                            let mut buf = std::mem::take(&mut php_stream.write_buffer);
                            buf.extend_from_slice(&bytes);
                            buf
                        } else {
                            bytes
                        };

                        let result = php_stream.stream.write_all(&to_write).await;
                        self.streams.insert(handle, php_stream);

                        match result {
                            Ok(_) => {
                                debug!("[{}] PHP fwrite (streaming): handle={}, wrote {} bytes immediately",
                                       self.request_domain, handle, len);
                                Ok(PhpValue::Int(len))
                            }
                            Err(e) => {
                                self.log_error(&format!("fwrite failed: {}", e));
                                Ok(PhpValue::Bool(false))
                            }
                        }
                    }
                    // For small writes (< 256 bytes) in non-streaming mode, buffer them
                    else if bytes.len() < 256 {
                        php_stream.write_buffer.extend_from_slice(&bytes);
                        debug!("[{}] PHP fwrite: handle={}, buffered {} bytes (total buffer: {})",
                               self.request_domain, handle, len, php_stream.write_buffer.len());

                        // Flush if buffer is getting large (> 1KB)
                        if php_stream.write_buffer.len() > 1024 {
                            let flush_data = std::mem::take(&mut php_stream.write_buffer);
                            match php_stream.stream.write_all(&flush_data).await {
                                Ok(_) => {
                                    debug!("[{}] PHP fwrite: flushed {} bytes", self.request_domain, flush_data.len());
                                }
                                Err(e) => {
                                    self.log_error(&format!("fwrite flush failed: {}", e));
                                    self.streams.insert(handle, php_stream);
                                    return Ok(PhpValue::Bool(false));
                                }
                            }
                        }
                        self.streams.insert(handle, php_stream);
                        Ok(PhpValue::Int(len))
                    } else {
                        // Large write - flush buffer first, then write directly
                        let to_write = if !php_stream.write_buffer.is_empty() {
                            let mut buf = std::mem::take(&mut php_stream.write_buffer);
                            buf.extend_from_slice(&bytes);
                            buf
                        } else {
                            bytes
                        };

                        let result = php_stream.stream.write_all(&to_write).await;
                        self.streams.insert(handle, php_stream);

                        match result {
                            Ok(_) => {
                                debug!("[{}] PHP fwrite: OK, wrote {} bytes (flushed: {})",
                                       self.request_domain, len, to_write.len());
                                Ok(PhpValue::Int(len))
                            }
                            Err(e) => {
                                self.log_error(&format!("fwrite failed: {}", e));
                                Ok(PhpValue::Bool(false))
                            }
                        }
                    }
                } else {
                    debug!("[{}] PHP fwrite: handle {} not found", self.request_domain, handle);
                    Ok(PhpValue::Bool(false))
                }
            }
            "fread" => {
                let handle = if let Some(PhpValue::Resource(id)) = args.first() {
                    *id
                } else {
                    0
                };
                let length = args
                    .get(1)
                    .map_or(8192usize, |a| a.as_int().max(1) as usize);
                if handle == 0 {
                    return Ok(PhpValue::Bool(false));
                }
                if let Some(mut php_stream) = self.streams.remove(&handle) {
                    let result = if php_stream.eof && php_stream.read_buffer.is_empty() {
                        Ok(PhpValue::String(String::new()))
                    } else {
                        // First check if we have enough buffered data
                        if php_stream.read_buffer.len() >= length {
                            // We have enough buffered data, return it immediately
                            let data: Vec<u8> = php_stream.read_buffer.drain(..length).collect();
                            let data_str = String::from_utf8_lossy(&data).into_owned();
                            debug!("[{}] PHP fread: handle={}, {} bytes from buffer", self.request_domain, handle, length);
                            Ok(PhpValue::String(data_str))
                        } else {
                            // Take what we have from buffer
                            let mut data: Vec<u8> = php_stream.read_buffer.drain(..).collect();
                            let remaining = length - data.len();

                            if remaining > 0 && !php_stream.eof {
                                // Need to read more from stream
                                let mut buf = vec![0u8; remaining];
                                // Short timeout simulates non-blocking read
                                match tokio::time::timeout(
                                    std::time::Duration::from_millis(10),
                                    php_stream.stream.read(&mut buf),
                                )
                                .await
                                {
                                    Ok(Ok(0)) => {
                                        php_stream.eof = true;
                                        debug!("[{}] PHP fread: handle={}, EOF after {} bytes", self.request_domain, handle, data.len());
                                    }
                                    Ok(Ok(n)) => {
                                        // Add the newly read data
                                        data.extend_from_slice(&buf[..n]);
                                        debug!("[{}] PHP fread: handle={}, {} bytes total (buffered + read)", self.request_domain, handle, data.len());
                                    }
                                    Ok(Err(e)) => {
                                        self.log_error(&format!("fread error: {}", e));
                                        php_stream.eof = true;
                                    }
                                    Err(_) => {
                                        // Timeout: no additional data available (non-blocking)
                                        debug!("[{}] PHP fread: handle={}, timeout, returning {} buffered bytes", self.request_domain, handle, data.len());
                                    }
                                }
                            }

                            if data.is_empty() && php_stream.eof {
                                Ok(PhpValue::String(String::new()))
                            } else {
                                let data_str = String::from_utf8_lossy(&data).into_owned();
                                Ok(PhpValue::String(data_str))
                            }
                        }
                    };
                    self.streams.insert(handle, php_stream);
                    result
                } else {
                    Ok(PhpValue::Bool(false))
                }
            }
            "fgets" => {
                // fgets($handle, $length = 1024) - read until newline or length
                let handle = if let Some(PhpValue::Resource(id)) = args.first() {
                    *id
                } else {
                    0
                };
                let max_length = args
                    .get(1)
                    .map_or(1024usize, |a| a.as_int().max(1) as usize);

                if handle == 0 {
                    return Ok(PhpValue::Bool(false));
                }

                if let Some(mut php_stream) = self.streams.remove(&handle) {
                    let result = if php_stream.eof && php_stream.read_buffer.is_empty() {
                        Ok(PhpValue::Bool(false))
                    } else {
                        // Check if we already have a newline in the buffer
                        if let Some(newline_pos) = php_stream.read_buffer.iter().position(|&b| b == b'\n') {
                            // Found newline in buffer, return line including newline
                            let line_end = (newline_pos + 1).min(max_length).min(php_stream.read_buffer.len());
                            let line: Vec<u8> = php_stream.read_buffer.drain(..line_end).collect();
                            let line_str = String::from_utf8_lossy(&line).into_owned();
                            debug!("[{}] PHP fgets: handle={}, buffered line: {:?}", self.request_domain, handle, &line_str);
                            Ok(PhpValue::String(line_str))
                        } else if php_stream.read_buffer.len() >= max_length {
                            // Buffer has enough data already
                            let line: Vec<u8> = php_stream.read_buffer.drain(..max_length).collect();
                            let line_str = String::from_utf8_lossy(&line).into_owned();
                            Ok(PhpValue::String(line_str))
                        } else {
                            // Need to read more data
                            let mut temp_buf = vec![0u8; 256]; // Read in small chunks for efficiency
                            let mut line: Vec<u8> = php_stream.read_buffer.drain(..).collect();

                            // Read until we find newline or reach max_length
                            loop {
                                if line.len() >= max_length {
                                    // Reached max length, return what we have
                                    let result = String::from_utf8_lossy(&line[..max_length]).into_owned();
                                    // Put remaining bytes back in buffer
                                    if line.len() > max_length {
                                        php_stream.read_buffer.extend_from_slice(&line[max_length..]);
                                    }
                                    break Ok(PhpValue::String(result));
                                }

                                // Try to read more data with a short timeout
                                match tokio::time::timeout(
                                    std::time::Duration::from_millis(100),
                                    php_stream.stream.read(&mut temp_buf),
                                )
                                .await
                                {
                                    Ok(Ok(0)) => {
                                        // EOF reached
                                        php_stream.eof = true;
                                        if line.is_empty() {
                                            break Ok(PhpValue::Bool(false));
                                        } else {
                                            let result = String::from_utf8_lossy(&line).into_owned();
                                            break Ok(PhpValue::String(result));
                                        }
                                    }
                                    Ok(Ok(n)) => {
                                        // Check for newline in newly read data
                                        if let Some(newline_pos) = temp_buf[..n].iter().position(|&b| b == b'\n') {
                                            // Found newline
                                            let bytes_needed = max_length - line.len();
                                            let take = (newline_pos + 1).min(bytes_needed);
                                            line.extend_from_slice(&temp_buf[..take]);

                                            // Buffer remaining data
                                            if take < n {
                                                php_stream.read_buffer.extend_from_slice(&temp_buf[take..n]);
                                            }
                                            let result = String::from_utf8_lossy(&line).into_owned();
                                            break Ok(PhpValue::String(result));
                                        } else {
                                            // No newline yet, add to line
                                            let bytes_needed = max_length - line.len();
                                            let take = n.min(bytes_needed);
                                            line.extend_from_slice(&temp_buf[..take]);

                                            // Buffer any excess
                                            if take < n {
                                                php_stream.read_buffer.extend_from_slice(&temp_buf[take..n]);
                                                // Check if we've reached max_length
                                                if line.len() >= max_length {
                                                    let result = String::from_utf8_lossy(&line).into_owned();
                                                    break Ok(PhpValue::String(result));
                                                }
                                            }
                                        }
                                    }
                                    Ok(Err(e)) => {
                                        self.log_error(&format!("fgets read error: {}", e));
                                        php_stream.eof = true;
                                        if line.is_empty() {
                                            break Ok(PhpValue::Bool(false));
                                        } else {
                                            let result = String::from_utf8_lossy(&line).into_owned();
                                            break Ok(PhpValue::String(result));
                                        }
                                    }
                                    Err(_) => {
                                        // Timeout - return what we have if anything
                                        if line.is_empty() {
                                            break Ok(PhpValue::String(String::new()));
                                        } else {
                                            let result = String::from_utf8_lossy(&line).into_owned();
                                            break Ok(PhpValue::String(result));
                                        }
                                    }
                                }
                            }
                        }
                    };
                    self.streams.insert(handle, php_stream);
                    result
                } else {
                    Ok(PhpValue::Bool(false))
                }
            }
            "feof" => {
                let handle = if let Some(PhpValue::Resource(id)) = args.first() {
                    *id
                } else {
                    0
                };
                if handle == 0 {
                    return Ok(PhpValue::Bool(true));
                }
                let eof = self.streams.get(&handle).map_or(true, |s| s.eof && s.read_buffer.is_empty());
                Ok(PhpValue::Bool(eof))
            }
            "fflush" => {
                let handle = if let Some(PhpValue::Resource(id)) = args.first() {
                    *id
                } else {
                    0
                };
                if handle == 0 {
                    return Ok(PhpValue::Bool(false));
                }

                if let Some(mut php_stream) = self.streams.remove(&handle) {
                    let result = if !php_stream.write_buffer.is_empty() {
                        let flush_data = std::mem::take(&mut php_stream.write_buffer);
                        debug!("[{}] PHP fflush: handle={}, flushing {} bytes", self.request_domain, handle, flush_data.len());
                        match php_stream.stream.write_all(&flush_data).await {
                            Ok(_) => Ok(PhpValue::Bool(true)),
                            Err(e) => {
                                self.log_error(&format!("fflush failed: {}", e));
                                Ok(PhpValue::Bool(false))
                            }
                        }
                    } else {
                        Ok(PhpValue::Bool(true))
                    };
                    self.streams.insert(handle, php_stream);
                    result
                } else {
                    Ok(PhpValue::Bool(false))
                }
            }
            "fclose" => {
                let handle = if let Some(PhpValue::Resource(id)) = args.first() {
                    *id
                } else {
                    0
                };

                // Flush write buffer before closing
                if let Some(mut php_stream) = self.streams.remove(&handle) {
                    if !php_stream.write_buffer.is_empty() {
                        let flush_data = std::mem::take(&mut php_stream.write_buffer);
                        debug!("[{}] PHP fclose: handle={}, flushing {} bytes before close",
                               self.request_domain, handle, flush_data.len());
                        let _ = php_stream.stream.write_all(&flush_data).await;
                    }
                    Ok(PhpValue::Bool(true))
                } else {
                    Ok(PhpValue::Bool(true))
                }
            }
            "php_uname" => Ok(PhpValue::String(std::env::consts::OS.to_string())),
            "php_sapi_name" => Ok(PhpValue::String("ruph-ast".to_string())),
            "ini_get" | "ini_set" => Ok(PhpValue::String(String::new())),

            // ── curl_* emulation via reqwest ──────────────────────────
            "curl_init" => {
                let id = self.next_curl_id;
                self.next_curl_id += 1;
                let mut handle = PhpCurlHandle::default();
                handle.method = "GET".to_string();
                // Optional URL argument
                if let Some(url) = args.first() {
                    handle.url = url.as_string();
                }
                self.curl_handles.insert(id, handle);
                Ok(PhpValue::Resource(id))
            }
            "curl_setopt" => {
                let id = if let Some(PhpValue::Resource(id)) = args.first() { *id } else { 0 };
                let opt = args.get(1).map_or(0, |a| a.as_int());
                let val = args.get(2).cloned().unwrap_or(PhpValue::Null);
                if let Some(handle) = self.curl_handles.get_mut(&id) {
                    match opt {
                        10002 => handle.url = val.as_string(),          // CURLOPT_URL
                        19913 => handle.return_transfer = val.is_truthy(), // CURLOPT_RETURNTRANSFER
                        47 => { if val.is_truthy() { handle.method = "POST".to_string(); } } // CURLOPT_POST
                        10015 => handle.post_fields = val.as_string(),  // CURLOPT_POSTFIELDS
                        10023 => {                                       // CURLOPT_HTTPHEADER
                            if let PhpValue::Array(items) = &val {
                                for item in items {
                                    let s = match item {
                                        PhpArrayItem::Value(v) => v.as_string(),
                                        PhpArrayItem::KeyValue(_, v) => v.as_string(),
                                    };
                                    if let Some((k, v)) = s.split_once(':') {
                                        handle.headers.push((k.trim().to_string(), v.trim().to_string()));
                                    }
                                }
                            }
                        }
                        52 => handle.follow_location = val.is_truthy(), // CURLOPT_FOLLOWLOCATION
                        13 => handle.timeout = val.as_int().max(0) as u64, // CURLOPT_TIMEOUT
                        10036 => handle.method = val.as_string().to_uppercase(), // CURLOPT_CUSTOMREQUEST
                        _ => {} // Silently ignore unsupported options
                    }
                }
                Ok(PhpValue::Bool(true))
            }
            "curl_setopt_array" => {
                let id = if let Some(PhpValue::Resource(id)) = args.first() { *id } else { 0 };
                if let Some(PhpValue::Array(items)) = args.get(1) {
                    for item in items {
                        if let PhpArrayItem::KeyValue(k, v) = item {
                            let opt = k.parse::<i64>().unwrap_or(0);
                            if let Some(handle) = self.curl_handles.get_mut(&id) {
                                match opt {
                                    10002 => handle.url = v.as_string(),
                                    19913 => handle.return_transfer = v.is_truthy(),
                                    47 => { if v.is_truthy() { handle.method = "POST".to_string(); } }
                                    10015 => handle.post_fields = v.as_string(),
                                    10023 => {
                                        if let PhpValue::Array(hdr_items) = v {
                                            for hi in hdr_items {
                                                let s = match hi {
                                                    PhpArrayItem::Value(hv) => hv.as_string(),
                                                    PhpArrayItem::KeyValue(_, hv) => hv.as_string(),
                                                };
                                                if let Some((hk, hv)) = s.split_once(':') {
                                                    handle.headers.push((hk.trim().to_string(), hv.trim().to_string()));
                                                }
                                            }
                                        }
                                    }
                                    52 => handle.follow_location = v.is_truthy(),
                                    13 => handle.timeout = v.as_int().max(0) as u64,
                                    10036 => handle.method = v.as_string().to_uppercase(),
                                    _ => {}
                                }
                            }
                        }
                    }
                }
                Ok(PhpValue::Bool(true))
            }
            "curl_exec" => {
                let id = if let Some(PhpValue::Resource(id)) = args.first() { *id } else { 0 };
                let handle = self.curl_handles.get(&id).cloned();
                if let Some(handle) = handle {
                    debug!("[{}] PHP curl_exec: {} {} (return_transfer={})", self.request_domain, handle.method, handle.url, handle.return_transfer);
                    let method = reqwest::Method::from_bytes(handle.method.as_bytes())
                        .unwrap_or(reqwest::Method::GET);
                    let mut request = self.http_client.request(method, &handle.url);
                    for (k, v) in &handle.headers {
                        request = request.header(k.as_str(), v.as_str());
                    }
                    if !handle.post_fields.is_empty() {
                        request = request.body(handle.post_fields.clone());
                    }
                    if handle.timeout > 0 {
                        request = request.timeout(std::time::Duration::from_secs(handle.timeout));
                    }
                    match request.send().await {
                        Ok(response) => {
                            let status = response.status().as_u16();
                            let content_type = response.headers()
                                .get("content-type")
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or("")
                                .to_string();
                            let body = response.bytes().await
                                .map(|b| String::from_utf8_lossy(&b).to_string())
                                .unwrap_or_default();
                            // Store response metadata in handle
                            if let Some(h) = self.curl_handles.get_mut(&id) {
                                h.response_status = status;
                                h.response_content_type = content_type.clone();
                                h.last_error = String::new();
                            }
                            debug!("[{}] PHP curl_exec: HTTP {} ({}) body_len={}", self.request_domain, status, content_type, body.len());
                            if handle.return_transfer {
                                Ok(PhpValue::String(body))
                            } else {
                                self.side_effect_output = Some(body);
                                Ok(PhpValue::Bool(true))
                            }
                        }
                        Err(e) => {
                            self.log_error(&format!("curl_exec failed: {}", e));
                            if let Some(h) = self.curl_handles.get_mut(&id) {
                                h.last_error = e.to_string();
                            }
                            Ok(PhpValue::Bool(false))
                        }
                    }
                } else {
                    Ok(PhpValue::Bool(false))
                }
            }
            "curl_getinfo" => {
                let id = if let Some(PhpValue::Resource(id)) = args.first() { *id } else { 0 };
                let opt = args.get(1).map_or(0, |a| a.as_int());
                if let Some(handle) = self.curl_handles.get(&id) {
                    match opt {
                        2097154 => Ok(PhpValue::Int(handle.response_status as i64)), // CURLINFO_HTTP_CODE / CURLINFO_RESPONSE_CODE
                        1048594 => Ok(PhpValue::String(handle.response_content_type.clone())), // CURLINFO_CONTENT_TYPE
                        _ => Ok(PhpValue::Int(handle.response_status as i64)),
                    }
                } else {
                    Ok(PhpValue::Bool(false))
                }
            }
            "curl_error" => {
                let id = if let Some(PhpValue::Resource(id)) = args.first() { *id } else { 0 };
                if let Some(handle) = self.curl_handles.get(&id) {
                    Ok(PhpValue::String(handle.last_error.clone()))
                } else {
                    Ok(PhpValue::String(String::new()))
                }
            }
            "curl_errno" => Ok(PhpValue::Int(0)),
            "curl_close" => {
                let id = if let Some(PhpValue::Resource(id)) = args.first() { *id } else { 0 };
                self.curl_handles.remove(&id);
                Ok(PhpValue::Null)
            }

            // ── libxml / simplexml stubs ──────────────────────────────
            "libxml_use_internal_errors" => {
                // Accept the call, return previous setting (always false)
                Ok(PhpValue::Bool(false))
            }
            "libxml_clear_errors" => Ok(PhpValue::Null),
            "libxml_get_errors" => Ok(PhpValue::Array(Vec::new())),
            "simplexml_load_string" => {
                let xml_string = args.first().map_or(String::new(), |a| a.as_string());
                if xml_string.is_empty() {
                    Ok(PhpValue::Bool(false))
                } else {
                    match xml_parser::parse_xml_to_php_value(&xml_string) {
                        Ok(value) => Ok(value),
                        Err(e) => {
                            self.log_error(&format!("simplexml_load_string: {}", e));
                            Ok(PhpValue::Bool(false))
                        }
                    }
                }
            }
            "simplexml_load_file" => {
                let file_path = args.first().map_or(String::new(), |a| a.as_string());
                if file_path.is_empty() {
                    Ok(PhpValue::Bool(false))
                } else {
                    match self.resolve_local_path(&file_path) {
                        Ok(abs_path) => {
                            match std::fs::read_to_string(&abs_path) {
                                Ok(xml_string) => {
                                    match xml_parser::parse_xml_to_php_value(&xml_string) {
                                        Ok(value) => Ok(value),
                                        Err(e) => {
                                            self.log_error(&format!("simplexml_load_file: {}", e));
                                            Ok(PhpValue::Bool(false))
                                        }
                                    }
                                }
                                Err(e) => {
                                    self.log_error(&format!("simplexml_load_file: Failed to read {}: {}", abs_path.display(), e));
                                    Ok(PhpValue::Bool(false))
                                }
                            }
                        }
                        Err(e) => {
                            self.log_error(&format!("simplexml_load_file: Failed to resolve path {}: {}", file_path, e));
                            Ok(PhpValue::Bool(false))
                        }
                    }
                }
            }

            // ── is_object / class_exists / get_class ──────────────────
            "is_object" => Ok(PhpValue::Bool(matches!(args.first(), Some(PhpValue::Object(_))))),
            "get_class" => {
                match args.first() {
                    Some(PhpValue::Object(obj)) => Ok(PhpValue::String(obj.class_name.clone())),
                    _ => Ok(PhpValue::Bool(false)),
                }
            }
            "class_exists" => {
                let name = args.first().map_or(String::new(), |a| a.as_string());
                Ok(PhpValue::Bool(self.user_classes.contains_key(&name)))
            }
            "property_exists" => {
                let obj = args.first();
                let prop = args.get(1).map_or(String::new(), |a| a.as_string());
                match obj {
                    Some(PhpValue::Object(o)) => Ok(PhpValue::Bool(o.props.contains_key(&prop))),
                    _ => Ok(PhpValue::Bool(false)),
                }
            }
            "method_exists" => {
                let class_or_obj = args.first();
                let method = args.get(1).map_or(String::new(), |a| a.as_string()).to_lowercase();
                let class_name = match class_or_obj {
                    Some(PhpValue::Object(obj)) => Some(obj.class_name.clone()),
                    Some(PhpValue::String(s)) => Some(s.clone()),
                    _ => None,
                };
                if let Some(cn) = class_name {
                    let mut found = false;
                    let mut search = Some(cn);
                    while let Some(ref name) = search.clone() {
                        if let Some(cls) = self.user_classes.get(name) {
                            if cls.methods.contains_key(&method) {
                                found = true;
                                break;
                            }
                            search = cls.parent.clone();
                        } else {
                            break;
                        }
                    }
                    Ok(PhpValue::Bool(found))
                } else {
                    Ok(PhpValue::Bool(false))
                }
            }

            "__DIR__" | "__FILE__" => Ok(PhpValue::String(
                self.current_template_path
                    .as_ref()
                    .and_then(|p| {
                        if func_name == "__DIR__" {
                            p.parent().map(|pp| pp.to_path_buf())
                        } else {
                            Some(p.clone())
                        }
                    })
                    .map_or(String::new(), |p| p.to_string_lossy().to_string()),
            )),

            // ── User-defined function call ──────────────────────────────
            _ => {
                if let Some(user_func) = self.user_functions.get(&func_name).cloned() {
                    debug!("[{}] PHP call user func: {}() with {} args", self.request_domain, func_name, args.len());
                    // Set up local scope with parameters
                    let saved_vars = self.variables.clone();
                    let saved_script_returned = self.script_returned.take();
                    let saved_body_override = self.response_body_override.take();
                    for (i, (param_name, default)) in user_func.params.iter().enumerate() {
                        let value = args
                            .get(i)
                            .cloned()
                            .or_else(|| default.clone())
                            .unwrap_or(PhpValue::Null);
                        self.variables.insert(param_name.clone(), value);
                    }

                    // Execute function body — call execute_php_ast directly
                    // to skip the regex tag-extraction pass.
                    let previous_source = self.source_code.clone();
                    let result = self.execute_php_ast(&user_func.body_source).await;
                    self.source_code = previous_source;

                    // Restore outer scope
                    self.variables = saved_vars;

                    // Extract function return value, then restore script-level state
                    let func_return_val = self.script_return_value.take();
                    let func_return = self.response_body_override.take();
                    self.script_returned = saved_script_returned;
                    self.response_body_override = saved_body_override;

                    match result {
                        Ok(func_output) => {
                            if let Some(rv) = func_return_val {
                                return Ok(rv);
                            }
                            if let Some(body_override) = func_return {
                                // return statement was hit inside function
                                return Ok(PhpValue::String(body_override));
                            }
                            if !func_output.is_empty() {
                                self.side_effect_output = Some(func_output);
                            }
                            Ok(PhpValue::Null)
                        }
                        Err(e) => Err(e),
                    }
                } else {
                    // Check constants
                    if let Some(val) = self.constants.get(&func_name) {
                        return Ok(val.clone());
                    }
                    warn!("[{}] {} Unknown function: {}() with {} args", self.request_domain, self.request_uri, func_name, args.len());
                    self.log_error_with_context(&format!("Unknown function: {}() with {} args", func_name, args.len()));
                    Ok(PhpValue::Null)
                }
            }
        }
    }

    /// Process a try/catch/finally statement.
    #[async_recursion]
    async fn process_try_catch(
        &mut self,
        node: Node<'async_recursion>,
        output: &mut String,
    ) -> Result<ControlFlow<()>> {
        // Find the try body (compound_statement that is the first named child)
        let body = node.child_by_field_name("body")
            .or_else(|| node.named_children(&mut node.walk())
                .find(|c| c.kind() == "compound_statement"));

        // Collect catch clauses and finally clause
        let mut catch_clauses = Vec::new();
        let mut finally_body = None;
        for child in node.named_children(&mut node.walk()) {
            match child.kind() {
                "catch_clause" => catch_clauses.push(child),
                "finally_clause" => {
                    finally_body = child.named_children(&mut child.walk())
                        .find(|c| c.kind() == "compound_statement");
                }
                _ => {}
            }
        }

        // Execute the try body
        let try_result = if let Some(body) = body {
            self.process_node(body, output).await
        } else {
            Ok(ControlFlow::Continue(()))
        };

        match try_result {
            Ok(flow) => {
                // Try body succeeded — run finally if present
                if let Some(fin) = finally_body {
                    let _ = self.process_node(fin, output).await?;
                }
                Ok(flow)
            }
            Err(e) => {
                // Check if this is a PhpThrow we can catch
                if let Some(thrown) = e.downcast_ref::<PhpThrow>() {
                    let thrown_class = thrown.class_name.clone();
                    let thrown_message = thrown.message.clone();
                    debug!("PHP try/catch: caught {} '{}'", thrown_class, thrown_message);

                    for catch_node in &catch_clauses {
                        // Extract the type list from catch clause
                        let type_list = catch_node.named_children(&mut catch_node.walk())
                            .find(|c| c.kind() == "type_list");
                        let mut matched = false;

                        if let Some(type_list) = type_list {
                            for type_node in type_list.named_children(&mut type_list.walk()) {
                                let type_name = type_node
                                    .utf8_text(self.source_code.as_bytes())
                                    .unwrap_or("")
                                    .trim_start_matches('\\')
                                    .to_string();
                                if type_name == thrown_class
                                    || type_name == "Exception"
                                    || type_name == "Throwable"
                                    || type_name == "\\Exception"
                                    || type_name == "\\Throwable"
                                {
                                    matched = true;
                                    break;
                                }
                                // Walk inheritance chain
                                let mut search = Some(thrown_class.clone());
                                while let Some(ref cn) = search.clone() {
                                    if cn == &type_name {
                                        matched = true;
                                        break;
                                    }
                                    search = self.user_classes.get(cn).and_then(|c| c.parent.clone());
                                }
                                if matched { break; }
                            }
                        } else {
                            // No type list = catch all
                            matched = true;
                        }

                        if matched {
                            // Find the catch variable name ($e)
                            let var_node = catch_node.named_children(&mut catch_node.walk())
                                .find(|c| c.kind() == "variable_name");
                            if let Some(var_node) = var_node {
                                let var_name = var_node
                                    .utf8_text(self.source_code.as_bytes())
                                    .unwrap_or("$e")
                                    .trim_start_matches('$')
                                    .to_string();
                                debug!("[{}] PHP catch: {}='{}' into ${}", self.request_domain, thrown_class, thrown_message, var_name);
                                // Create an exception object with a getMessage() method
                                let mut props = HashMap::new();
                                props.insert("message".to_string(), PhpValue::String(thrown_message.clone()));
                                let ex_obj = PhpObject {
                                    class_name: thrown_class.clone(),
                                    props,
                                };
                                self.variables.insert(var_name, PhpValue::Object(Box::new(ex_obj)));
                            }

                            // Execute the catch body
                            let catch_body = catch_node.named_children(&mut catch_node.walk())
                                .find(|c| c.kind() == "compound_statement");
                            let result = if let Some(cb) = catch_body {
                                self.process_node(cb, output).await
                            } else {
                                Ok(ControlFlow::Continue(()))
                            };

                            if let Some(fin) = finally_body {
                                let _ = self.process_node(fin, output).await?;
                            }
                            return result;
                        }
                    }
                    // No catch matched — run finally and re-throw
                    if let Some(fin) = finally_body {
                        let _ = self.process_node(fin, output).await?;
                    }
                    Err(e)
                } else {
                    // Not a PhpThrow — run finally and propagate
                    if let Some(fin) = finally_body {
                        let _ = self.process_node(fin, output).await;
                    }
                    Err(e)
                }
            }
        }
    }

    /// Execute a PHP method on an object. Returns (return_value, updated_object).
    /// The updated object reflects any `$this->prop = …` mutations during execution.
    async fn call_method(
        &mut self,
        obj: PhpObject,
        method_name: &str,
        args: Vec<PhpValue>,
    ) -> Result<(PhpValue, PhpObject)> {
        // Walk the class hierarchy to find the method
        let class_name = obj.class_name.clone();
        let method = {
            let mut found: Option<PhpFunction> = None;
            let mut search: Option<String> = Some(class_name.clone());
            while let Some(ref cn) = search.clone() {
                if let Some(cls) = self.user_classes.get(cn) {
                    if let Some(m) = cls.methods.get(method_name) {
                        found = Some(m.clone());
                        break;
                    }
                    search = cls.parent.clone();
                } else {
                    break;
                }
            }
            found
        };

        let Some(method) = method else {
            // Built-in object methods (Exception, etc.)
            match method_name {
                "getmessage" => {
                    let msg = obj.props.get("message").cloned().unwrap_or(PhpValue::Null);
                    debug!("[{}] PHP getMessage() on {}: props={:?}, result={:?}", self.request_domain, class_name, obj.props.keys().collect::<Vec<_>>(), msg);
                    return Ok((msg, obj));
                }
                "getcode" => {
                    let code = obj.props.get("code").cloned().unwrap_or(PhpValue::Int(0));
                    return Ok((code, obj));
                }
                "getfile" | "getline" | "gettrace" | "gettraceAsString" | "getprevious" => {
                    return Ok((PhpValue::String(String::new()), obj));
                }
                _ => {
                    self.log_error_with_context(&format!(
                        "Call to undefined method {}::{}()",
                        class_name, method_name
                    ));
                    return Ok((PhpValue::Null, obj));
                }
            }
        };

        // Save outer execution state
        let saved_vars = self.variables.clone();
        let saved_script_returned = self.script_returned.take();
        let saved_return_value = self.script_return_value.take();
        let saved_body_override = self.response_body_override.take();

        // Bind parameters and $this into the current variable scope
        for (i, (param_name, default)) in method.params.iter().enumerate() {
            let value = args
                .get(i)
                .cloned()
                .or_else(|| default.clone())
                .unwrap_or(PhpValue::Null);
            self.variables.insert(param_name.clone(), value);
        }
        self.variables
            .insert("this".to_string(), PhpValue::Object(Box::new(obj.clone())));

        // Execute the method body — call execute_php_ast directly to skip
        // the regex tag-extraction pass that process_php_code does.
        let previous_source = self.source_code.clone();
        let method_output;
        let exec_result = self.execute_php_ast(&method.body_source).await;
        match &exec_result {
            Ok(out) => method_output = out.clone(),
            Err(e) => {
                debug!("[{}] PHP method {}::{}() error: {}", self.request_domain, class_name, method_name, e);
                method_output = String::new();
            }
        }
        self.source_code = previous_source;

        // Capture the (possibly mutated) $this and the typed return value
        let updated_obj = match self.variables.remove("this") {
            Some(PhpValue::Object(o)) => *o,
            _ => obj,
        };
        let return_val = self.script_return_value.take().unwrap_or(PhpValue::Null);

        // Restore outer state
        self.variables = saved_vars;
        self.script_returned = saved_script_returned;
        self.script_return_value = saved_return_value;
        self.response_body_override = saved_body_override;

        // Propagate any echoed output from the method as a side-effect
        if !method_output.is_empty() {
            let combined = match self.side_effect_output.take() {
                Some(existing) => existing + &method_output,
                None => method_output,
            };
            self.side_effect_output = Some(combined);
        }

        exec_result?;
        Ok((return_val, updated_obj))
    }

    fn reset_request_state(&mut self, template_path: &Path, root_dir: &Path) {
        self.variables = self.global_variables.clone();
        self.superglobals.clear();
        self.response_status = 200;
        self.response_headers.clear();
        self.response_body_override = None;
        self.script_returned = None;
        self.script_return_value = None;
        self.streams.clear();
        self.next_stream_id = 1;
        self.current_template_path = Some(template_path.to_path_buf());
        self.root_dir = Some(root_dir.to_path_buf());
        self.included_files.clear();
        self.output_buffers.clear();
        self.side_effect_output = None;
        self.session_id = None;
        self.session_destroyed = false;
        self.session_lock = None;
        self.curl_handles.clear();
        self.next_curl_id = 1;
        // Preserve user_functions, user_classes, and constants across requests (like opcache)
    }

    fn parse_cookie_header(server_vars: &HashMap<String, String>) -> HashMap<String, String> {
        let mut cookies = HashMap::new();
        let Some(raw) = server_vars.get("HTTP_COOKIE") else {
            return cookies;
        };

        for pair in raw.split(';') {
            let pair = pair.trim();
            if pair.is_empty() {
                continue;
            }
            let Some((name, value)) = pair.split_once('=') else {
                continue;
            };
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            let value = urlencoding::decode(value.trim())
                .map(|v| v.to_string())
                .unwrap_or_else(|_| value.trim().to_string());
            cookies.insert(name.to_string(), value);
        }

        cookies
    }

    fn add_response_header(&mut self, name: &str, value: String) {
        let name = name.trim().to_ascii_lowercase();
        if name == "set-cookie" {
            if !self.response_headers.contains_key("set-cookie") {
                self.response_headers
                    .insert("set-cookie".to_string(), value);
                return;
            }

            let mut idx = 2;
            loop {
                let key = format!("set-cookie-{}", idx);
                if !self.response_headers.contains_key(&key) {
                    self.response_headers.insert(key, value);
                    return;
                }
                idx += 1;
            }
        }

        self.response_headers.insert(name, value);
    }

    fn is_valid_session_id(id: &str) -> bool {
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    }

    fn session_file_path(id: &str) -> PathBuf {
        std::env::temp_dir()
            .join("ruph_sessions")
            .join(format!("sess_{}", id))
    }

    async fn acquire_session_lock(id: &str) -> OwnedMutexGuard<()> {
        let lock = {
            let mut locks = SESSION_LOCKS
                .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
                .lock()
                .unwrap();
            locks
                .entry(id.to_string())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        lock.lock_owned().await
    }

    async fn ruph_session_start(&mut self, args: &[PhpValue]) -> Result<PhpValue> {
        let Some(PhpValue::String(id)) = args.first() else {
            self.log_error("ruph_session_start() requires a string session id");
            return Ok(PhpValue::Bool(false));
        };
        if !Self::is_valid_session_id(id) {
            self.log_error("ruph_session_start() received an invalid session id");
            return Ok(PhpValue::Bool(false));
        }

        if self.session_id.as_deref() == Some(id) && self.session_lock.is_some() {
            self.add_response_header(
                "set-cookie",
                format!("RUPHSESSID={}; Path=/; HttpOnly; SameSite=Lax", id),
            );
            return Ok(PhpValue::Bool(true));
        }

        self.finalize_session().await;
        self.session_lock = None;
        self.session_lock = Some(Self::acquire_session_lock(id).await);

        let path = Self::session_file_path(id);
        let mut session = HashMap::new();
        if let Ok(raw) = fs::read_to_string(&path).await {
            match serde_json::from_str::<HashMap<String, String>>(&raw) {
                Ok(values) => session = values,
                Err(e) => self.log_error(&format!("Failed to parse ruph session {}: {}", id, e)),
            }
        }

        self.session_id = Some(id.clone());
        self.session_destroyed = false;
        self.superglobals.insert("_SESSION".to_string(), session);
        self.add_response_header(
            "set-cookie",
            format!("RUPHSESSID={}; Path=/; HttpOnly; SameSite=Lax", id),
        );
        Ok(PhpValue::Bool(true))
    }

    async fn ruph_session_destroy(&mut self) -> Result<PhpValue> {
        if let Some(id) = self.session_id.clone() {
            let path = Self::session_file_path(&id);
            if let Err(e) = fs::remove_file(&path).await {
                if e.kind() != std::io::ErrorKind::NotFound {
                    self.log_error(&format!("Failed to remove ruph session {}: {}", id, e));
                    return Ok(PhpValue::Bool(false));
                }
            }
        }

        self.session_destroyed = true;
        self.superglobals
            .insert("_SESSION".to_string(), HashMap::new());
        self.add_response_header(
            "set-cookie",
            "RUPHSESSID=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax".to_string(),
        );
        self.session_lock = None;
        Ok(PhpValue::Bool(true))
    }

    async fn finalize_session(&mut self) {
        let Some(id) = self.session_id.clone() else {
            self.session_lock = None;
            return;
        };
        if self.session_destroyed {
            self.session_lock = None;
            return;
        }

        let path = Self::session_file_path(&id);
        if let Some(parent) = path.parent() {
            if let Err(e) = fs::create_dir_all(parent).await {
                self.log_error(&format!("Failed to create ruph session directory: {}", e));
                self.session_lock = None;
                return;
            }
        }

        let session = self
            .superglobals
            .get("_SESSION")
            .cloned()
            .unwrap_or_default();
        match serde_json::to_string(&session) {
            Ok(raw) => {
                if let Err(e) = fs::write(&path, raw).await {
                    self.log_error(&format!("Failed to write ruph session {}: {}", id, e));
                }
            }
            Err(e) => self.log_error(&format!("Failed to serialize ruph session {}: {}", id, e)),
        }
        self.session_lock = None;
    }

    /// Execute a chain of PHP scripts as a single request context.
    ///
    /// All scripts share variables, constants, functions, and the include-once registry —
    /// identical to how PHP itself handles a single entry script that includes others.
    ///
    /// Controllers (master, vhost-root) stop the chain on exit, return, or auto-handled output.
    /// Leaf (_index.php) runs last; `return true` means pass-through, anything else = handled.
    pub async fn execute_chain(
        &mut self,
        controllers: &[(PathBuf, HashMap<String, String>)],
        leaf: Option<&(PathBuf, HashMap<String, String>)>,
        get_params: &HashMap<String, String>,
        post_params: &HashMap<String, String>,
        root_dir: &Path,
        error_handler: Option<Arc<dyn Fn(&str) + Send + Sync>>,
    ) -> Result<PhpExecution> {
        let first = controllers
            .first()
            .map(|(p, sv)| (p, sv))
            .or_else(|| leaf.map(|(p, sv)| (p, sv)));

        let Some((first_path, first_sv)) = first else {
            return Ok(PhpExecution {
                body: String::new(),
                status: 200,
                headers: HashMap::new(),
                exited: false,
                returned: None,
            });
        };

        // Single reset for the entire request chain
        self.reset_request_state(first_path, root_dir);
        self.error_log_handler = error_handler;
        self.accumulated_output.clear();

        // Extract domain/URI from server vars for log context
        self.request_domain = first_sv
            .get("HTTP_HOST")
            .cloned()
            .unwrap_or_default();
        self.request_uri = first_sv
            .get("REQUEST_URI")
            .cloned()
            .unwrap_or_default();

        self.superglobals
            .insert("_GET".to_string(), get_params.clone());
        self.superglobals
            .insert("_POST".to_string(), post_params.clone());
        self.superglobals
            .insert("_SERVER".to_string(), first_sv.clone());
        self.superglobals
            .insert("_COOKIE".to_string(), Self::parse_cookie_header(first_sv));
        self.superglobals
            .insert("_SESSION".to_string(), HashMap::new());
        let mut req_params = get_params.clone();
        req_params.extend(post_params.clone());
        self.superglobals.insert("_REQUEST".to_string(), req_params);

        let mut total_output = String::new();
        let mut exited = false;
        let mut chain_stopped = false;
        let mut chain_returned: Option<bool> = None;

        warn!("[{}] PHP chain: {} controllers, leaf={}", self.request_domain, controllers.len(),
            leaf.map(|(p, _)| p.display().to_string()).unwrap_or_else(|| "none".to_string()));

        // ── Controllers ──────────────────────────────────────────────────────
        for (script_path, server_vars) in controllers {
            debug!("[{}] PHP chain: running controller {:?}", self.request_domain, script_path);
            self.current_template_path = Some(script_path.clone());
            self.superglobals
                .insert("_SERVER".to_string(), server_vars.clone());
            // Clear per-script signals; preserve variables/constants/headers/status
            self.script_returned = None;
            self.script_return_value = None;
            self.response_body_override = None;

            let content = fs::read_to_string(script_path)
                .await
                .map_err(|e| anyhow!("Cannot read {:?}: {}", script_path, e))?;

            let mut script_output = String::new();
            match self.process_php_code(&content, &mut script_output).await {
                Ok(_) => {
                    debug!("[{}] PHP chain: controller OK, output_len={}", self.request_domain, script_output.len());
                }
                Err(e) if e.downcast_ref::<PhpExit>().is_some() => {
                    debug!("[{}] PHP chain: controller exited (PhpExit), output_len={}", self.request_domain, script_output.len());
                    exited = true;
                    chain_stopped = true;
                    total_output.push_str(&script_output);
                    if total_output.is_empty() && !self.accumulated_output.is_empty() {
                        total_output = std::mem::take(&mut self.accumulated_output);
                    }
                    break;
                }
                Err(e) => {
                    warn!("[{}] PHP chain: controller error: {}", self.request_domain, e);
                    return Err(e);
                }
            }

            total_output.push_str(&script_output);

            // Stop if script returned a value, produced output, changed status, or set Location
            let auto_handled = !script_output.trim().is_empty()
                || self.response_status != 200
                || self.response_headers.contains_key("location");

            if self.script_returned.is_some() || auto_handled {
                chain_returned = self.script_returned;
                chain_stopped = true;
                break;
            }
        }

        // ── Leaf ─────────────────────────────────────────────────────────────
        if !chain_stopped {
            if let Some((leaf_path, leaf_sv)) = leaf {
                debug!("[{}] PHP chain: running leaf {:?}", self.request_domain, leaf_path);
                self.current_template_path = Some(leaf_path.clone());
                self.superglobals
                    .insert("_SERVER".to_string(), leaf_sv.clone());
                self.script_returned = None;
                self.script_return_value = None;
                self.response_body_override = None;

                let content = fs::read_to_string(leaf_path)
                    .await
                    .map_err(|e| anyhow!("Cannot read {:?}: {}", leaf_path, e))?;

                let mut leaf_output = String::new();
                match self.process_php_code(&content, &mut leaf_output).await {
                    Ok(_) => {
                        total_output.push_str(&leaf_output);
                        chain_returned = self.script_returned;
                    }
                    Err(e) if e.downcast_ref::<PhpExit>().is_some() => {
                        exited = true;
                        total_output.push_str(&leaf_output);
                        if total_output.is_empty() && !self.accumulated_output.is_empty() {
                            total_output = std::mem::take(&mut self.accumulated_output);
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
        }

        // ── Final streaming flush ─────────────────────────────────────────────
        // If in streaming mode, commit headers (if not yet sent) and flush remaining output
        if let Some(tx) = self.flush_tx.clone() {
            if let Some(htx) = self.headers_tx.take() {
                let _ = htx.send((self.response_headers.clone(), self.response_status));
            }
            let chunk = std::mem::take(&mut self.streaming_buf);
            if !chunk.is_empty() {
                let _ = tx.send(Ok(Bytes::from(chunk.into_bytes()))).await;
            }
            // flush_tx drop happens when AstPhpProcessor is dropped, closing the channel
        }

        // return-signals (true/false) are not response content — use accumulated echo output
        let body = if chain_returned.is_some() {
            total_output
        } else {
            self.response_body_override.clone().unwrap_or(total_output)
        };

        let status =
            if self.response_status == 200 && self.response_headers.contains_key("location") {
                302
            } else {
                self.response_status
            };

        self.finalize_session().await;

        Ok(PhpExecution {
            body,
            status,
            headers: self.response_headers.clone(),
            exited,
            returned: chain_returned,
        })
    }

    /// Set up streaming mode: echo output is buffered and flushed to `flush_tx` on each flush() call.
    /// `headers_tx` receives response headers on the first flush() (or end of chain).
    pub fn init_streaming(
        &mut self,
        flush_tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
        headers_tx: oneshot::Sender<(HashMap<String, String>, u16)>,
    ) {
        self.flush_tx = Some(flush_tx);
        self.headers_tx = Some(headers_tx);
        self.streaming_buf = String::new();
    }

    /// Copy compiled state (functions, constants) from another processor.
    /// Used to seed a fresh streaming processor with already-parsed definitions.
    pub fn borrow_compiled_state(&mut self, other: &AstPhpProcessor) {
        self.user_functions = other.user_functions.clone();
        self.user_classes = other.user_classes.clone();
        self.constants = other.constants.clone();
        self.global_variables = other.global_variables.clone();
    }

    pub async fn execute_init(
        &mut self,
        php_code: &str,
        server_vars: &HashMap<String, String>,
        template_path: &Path,
        root_dir: &Path,
    ) -> Result<()> {
        self.reset_request_state(template_path, root_dir);
        self.superglobals
            .insert("_SERVER".to_string(), server_vars.clone());

        let mut output = String::new();
        self.process_php_code(php_code, &mut output).await?;
        self.global_variables = self.variables.clone();
        Ok(())
    }

    async fn render_template(
        &mut self,
        template: &PhpValue,
        data: Option<&PhpValue>,
    ) -> Result<Option<String>> {
        let path = self.resolve_local_path(&template.as_string())?;
        let content = fs::read_to_string(&path)
            .await
            .map_err(|_| anyhow!("Cannot read template"))?;

        let previous_source = self.source_code.clone();
        let previous_template = self.current_template_path.clone();
        self.current_template_path = Some(path.clone());

        if let Some(data) = data {
            self.apply_data_to_vars(data);
        }

        let mut output = String::new();
        self.process_php_code(&content, &mut output).await?;
        self.source_code = previous_source;
        self.current_template_path = previous_template;
        Ok(Some(output))
    }

    async fn fetch_content(
        &mut self,
        target: &PhpValue,
        headers: Option<&PhpValue>,
    ) -> Result<PhpValue> {
        let target = target.as_string();
        if target == "php://input" {
            let body = self
                .superglobals
                .get("_SERVER")
                .and_then(|server| server.get("RUPH_RAW_BODY"))
                .cloned()
                .unwrap_or_default();
            return Ok(PhpValue::String(body));
        }
        if target.starts_with("http://") || target.starts_with("https://") {
            let mut request = self.http_client.get(&target);
            if let Some(headers) = headers {
                for (name, value) in self.extract_headers(headers) {
                    request = request.header(name, value);
                }
            }
            let response = request
                .send()
                .await
                .map_err(|e| anyhow!("HTTP request failed: {}", e))?;
            let bytes = response
                .bytes()
                .await
                .map_err(|e| anyhow!("Failed to read response: {}", e))?;
            return Ok(PhpValue::String(
                String::from_utf8_lossy(&bytes).to_string(),
            ));
        }

        let path = self.resolve_local_path(&target)?;
        let bytes = fs::read(&path)
            .await
            .map_err(|_| anyhow!("Cannot read file"))?;
        Ok(PhpValue::String(
            String::from_utf8_lossy(&bytes).to_string(),
        ))
    }

    async fn http_request(
        &mut self,
        method: &str,
        url: &str,
        headers: Option<&PhpValue>,
        body: &str,
    ) -> Result<PhpValue> {
        let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET);
        let mut request = self.http_client.request(method, url);
        if let Some(headers) = headers {
            for (name, value) in self.extract_headers(headers) {
                request = request.header(name, value);
            }
        }
        if !body.is_empty() {
            request = request.body(body.to_string());
        }
        let response = request
            .send()
            .await
            .map_err(|e| anyhow!("HTTP request failed: {}", e))?;
        let bytes = response
            .bytes()
            .await
            .map_err(|e| anyhow!("Failed to read response: {}", e))?;
        Ok(PhpValue::String(
            String::from_utf8_lossy(&bytes).to_string(),
        ))
    }

    fn resolve_local_path(&self, target: &str) -> Result<PathBuf> {
        let root_dir = self
            .root_dir
            .as_ref()
            .ok_or_else(|| anyhow!("Root dir not set"))?;
        let base_dir = self
            .current_template_path
            .as_ref()
            .and_then(|p| p.parent())
            .unwrap_or(root_dir);

        let canonical_root = root_dir
            .canonicalize()
            .map_err(|_| anyhow!("Cannot canonicalize root dir"))?;

        let raw_path = if target.starts_with('/') {
            let abs = PathBuf::from(target);
            // Absolute path — if the file already exists, canonicalize and return it.
            if let Ok(canonical) = abs.canonicalize() {
                return Ok(canonical);
            }
            // File may not exist yet (e.g. file_put_contents creating a new file).
            // Validate the parent directory exists.
            if let Some(parent) = abs.parent() {
                if parent.is_dir() {
                    return Ok(abs);
                }
            }
            // Fall back to treating as docroot-relative
            root_dir.join(target.trim_start_matches('/'))
        } else if target.starts_with("./") {
            base_dir.join(&target[2..])
        } else {
            base_dir.join(target)
        };

        let canonical_path = raw_path
            .canonicalize()
            .map_err(|_| anyhow!("Cannot resolve path: {}", raw_path.display()))?;

        if !canonical_path.starts_with(&canonical_root) {
            return Err(anyhow!("Path traversal attempt detected"));
        }

        Ok(canonical_path)
    }

    fn apply_header_block(&mut self, headers: &PhpValue) {
        for (name, value) in self.extract_headers(headers) {
            self.add_response_header(&name, value);
        }
    }

    fn extract_headers(&self, headers: &PhpValue) -> Vec<(String, String)> {
        match headers {
            PhpValue::Array(items) => {
                let mut pairs = Vec::new();
                for item in items {
                    match item {
                        PhpArrayItem::KeyValue(key, value) => {
                            pairs.push((key.clone(), value.as_string()));
                        }
                        PhpArrayItem::Value(value) => {
                            if let Some((name, value)) = value.as_string().split_once(':') {
                                pairs.push((name.trim().to_string(), value.trim().to_string()));
                            }
                        }
                    }
                }
                pairs
            }
            PhpValue::String(headers) => parse_header_block(headers),
            _ => Vec::new(),
        }
    }

    fn apply_data_to_vars(&mut self, data: &PhpValue) {
        match data {
            PhpValue::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    match item {
                        PhpArrayItem::KeyValue(key, value) => {
                            self.variables.insert(key.clone(), value.clone());
                        }
                        PhpArrayItem::Value(value) => {
                            self.variables.insert(index.to_string(), value.clone());
                        }
                    }
                }
            }
            PhpValue::String(json) => {
                if let Ok(json) = serde_json::from_str::<JsonValue>(json) {
                    if let Some(obj) = json.as_object() {
                        for (key, value) in obj {
                            if let Some(value) = value.as_str() {
                                self.variables
                                    .insert(key.clone(), PhpValue::String(value.to_string()));
                            } else {
                                self.variables
                                    .insert(key.clone(), PhpValue::String(value.to_string()));
                            }
                        }
                        return;
                    }
                }
                self.variables
                    .insert("data".to_string(), PhpValue::String(json.clone()));
            }
            _ => {
                self.variables.insert("data".to_string(), data.clone());
            }
        }
    }

    fn append_output(&mut self, output: &mut String, text: &str) {
        if let Some(buffer) = self.output_buffers.last_mut() {
            buffer.push_str(text);
        } else if self.flush_tx.is_some() {
            // Streaming mode: accumulate in streaming_buf; flush() drains it to the channel
            if !text.is_empty() {
                debug!("[{}] PHP echo (streaming): {} bytes: {}", self.request_domain, text.len(), &text[..text.len().min(200)]);
            }
            self.streaming_buf.push_str(text);
        } else {
            output.push_str(text);
        }
    }

    /// Handle array_map/array_filter with callback (arrow_function or anonymous_function).
    #[async_recursion]
    async fn call_array_callback_function(
        &mut self,
        func_name: &str,
        args_node: Node<'async_recursion>,
    ) -> Result<PhpValue> {
        let mut arg_nodes: Vec<Node> = Vec::new();
        for child in args_node.named_children(&mut args_node.walk()) {
            let n = if child.kind() == "argument" {
                child.named_child(0).unwrap_or(child)
            } else {
                child
            };
            arg_nodes.push(n);
        }

        if func_name == "array_map" {
            // array_map($callback, $array)
            let callback_node = arg_nodes.first().copied();
            let array_val = if let Some(n) = arg_nodes.get(1) {
                self.evaluate_expression(*n).await?
            } else {
                PhpValue::Array(vec![])
            };

            // If callback is null, just return the array
            let Some(cb_node) = callback_node else {
                return Ok(array_val);
            };
            if cb_node.kind() == "null" {
                return Ok(array_val);
            }

            let items = match array_val {
                PhpValue::Array(items) => items,
                _ => return Ok(PhpValue::Array(vec![])),
            };

            // Extract callback parameter info
            let (param_name, body_node, is_arrow) = self.extract_callback_info(cb_node)?;

            let mut result = Vec::new();
            for item in &items {
                let val = match item {
                    PhpArrayItem::Value(v) => v.clone(),
                    PhpArrayItem::KeyValue(_, v) => v.clone(),
                };
                // Bind parameter
                let saved = self.variables.get(&param_name).cloned();
                self.variables.insert(param_name.clone(), val);

                let mapped = if is_arrow {
                    // Arrow function: evaluate the body expression
                    self.evaluate_expression(body_node).await?
                } else {
                    // Anonymous function: process the body block
                    let mut out = String::new();
                    let _ = self.process_node(body_node, &mut out).await?;
                    self.script_return_value.take()
                        .or_else(|| self.response_body_override.take().map(PhpValue::String))
                        .unwrap_or(PhpValue::Null)
                };

                // Restore
                match saved {
                    Some(v) => { self.variables.insert(param_name.clone(), v); }
                    None => { self.variables.remove(&param_name); }
                }
                result.push(PhpArrayItem::Value(mapped));
            }
            Ok(PhpValue::Array(result))
        } else {
            // array_filter($array, $callback)
            let array_val = if let Some(n) = arg_nodes.first() {
                self.evaluate_expression(*n).await?
            } else {
                return Ok(PhpValue::Array(vec![]));
            };

            let items = match array_val {
                PhpValue::Array(items) => items,
                _ => return Ok(PhpValue::Array(vec![])),
            };

            let callback_node = arg_nodes.get(1).copied();
            if callback_node.is_none() {
                // No callback: filter out falsy values
                let filtered: Vec<PhpArrayItem> = items.into_iter().filter(|item| {
                    let v = match item {
                        PhpArrayItem::Value(v) => v,
                        PhpArrayItem::KeyValue(_, v) => v,
                    };
                    v.is_truthy()
                }).collect();
                return Ok(PhpValue::Array(filtered));
            }

            let cb_node = callback_node.unwrap();
            let (param_name, body_node, is_arrow) = self.extract_callback_info(cb_node)?;

            let mut result = Vec::new();
            for item in &items {
                let val = match item {
                    PhpArrayItem::Value(v) => v.clone(),
                    PhpArrayItem::KeyValue(_, v) => v.clone(),
                };
                let saved = self.variables.get(&param_name).cloned();
                self.variables.insert(param_name.clone(), val);

                let keep = if is_arrow {
                    self.evaluate_expression(body_node).await?.is_truthy()
                } else {
                    let mut out = String::new();
                    let _ = self.process_node(body_node, &mut out).await?;
                    self.script_return_value.take()
                        .or_else(|| self.response_body_override.take().map(PhpValue::String))
                        .map_or(false, |v| v.is_truthy())
                };

                match saved {
                    Some(v) => { self.variables.insert(param_name.clone(), v); }
                    None => { self.variables.remove(&param_name); }
                }
                if keep {
                    result.push(item.clone());
                }
            }
            Ok(PhpValue::Array(result))
        }
    }

    /// Extract parameter name and body from an arrow_function or anonymous_function node.
    fn extract_callback_info<'a>(&self, node: Node<'a>) -> Result<(String, Node<'a>, bool)> {
        match node.kind() {
            "arrow_function" => {
                // fn($x) => expr
                let params_node = node.child_by_field_name("parameters");
                let body = node.child_by_field_name("body")
                    .ok_or_else(|| anyhow!("arrow_function has no body"))?;
                let param_name = if let Some(params) = params_node {
                    let mut name = "v".to_string();
                    for child in params.named_children(&mut params.walk()) {
                        if child.kind() == "simple_parameter" || child.kind() == "parameter" {
                            if let Some(n) = child.child_by_field_name("name") {
                                name = n.utf8_text(self.source_code.as_bytes())
                                    .unwrap_or("$v")
                                    .trim_start_matches('$')
                                    .to_string();
                                break;
                            }
                        }
                    }
                    name
                } else {
                    "v".to_string()
                };
                Ok((param_name, body, true))
            }
            "anonymous_function" => {
                let params_node = node.child_by_field_name("parameters");
                let body = node.child_by_field_name("body")
                    .ok_or_else(|| anyhow!("anonymous_function has no body"))?;
                let param_name = if let Some(params) = params_node {
                    let mut name = "v".to_string();
                    for child in params.named_children(&mut params.walk()) {
                        if child.kind() == "simple_parameter" || child.kind() == "parameter" {
                            if let Some(n) = child.child_by_field_name("name") {
                                name = n.utf8_text(self.source_code.as_bytes())
                                    .unwrap_or("$v")
                                    .trim_start_matches('$')
                                    .to_string();
                                break;
                            }
                        }
                    }
                    name
                } else {
                    "v".to_string()
                };
                Ok((param_name, body, false))
            }
            _ => {
                // String callback name or null — just return a dummy
                Err(anyhow!("Expected arrow_function or anonymous_function, got {}", node.kind()))
            }
        }
    }

    /// Handle preg_match() and preg_match_all() — writes captures into the 3rd argument variable.
    async fn call_preg_match_family(
        &mut self,
        func_name: &str,
        args_node: Option<Node<'_>>,
    ) -> Result<PhpValue> {
        // Collect argument nodes
        let mut arg_nodes: Vec<Node> = Vec::new();
        if let Some(an) = args_node {
            for child in an.named_children(&mut an.walk()) {
                let n = if child.kind() == "argument" {
                    child.named_child(0).unwrap_or(child)
                } else {
                    child
                };
                arg_nodes.push(n);
            }
        }

        let pattern = if let Some(n) = arg_nodes.first() {
            self.evaluate_expression(*n).await?.as_string()
        } else {
            return Ok(PhpValue::Int(0));
        };

        let subject = if let Some(n) = arg_nodes.get(1) {
            self.evaluate_expression(*n).await?.as_string()
        } else {
            return Ok(PhpValue::Int(0));
        };

        let re_str = build_regex_with_flags(&pattern);
        let re = match Regex::new(&re_str) {
            Ok(r) => r,
            Err(e) => {
                self.log_error(&format!("preg regex error: {}", e));
                return Ok(PhpValue::Bool(false));
            }
        };

        // Optional 3rd arg: variable name to store captures
        let captures_var: Option<String> = arg_nodes.get(2).and_then(|n| self.get_identifier(*n));

        // Optional 4th arg: flags (PREG_PATTERN_ORDER=1, PREG_SET_ORDER=2)
        let flags = if let Some(n) = arg_nodes.get(3) {
            self.evaluate_expression(*n).await?.as_int()
        } else {
            0
        };
        let set_order = flags & 2 != 0; // PREG_SET_ORDER

        if func_name == "preg_match_all" {
            let num_groups = re.captures_len();
            if set_order {
                // PREG_SET_ORDER: each match is [full_match, group1, group2, ...]
                let mut matches: Vec<PhpArrayItem> = Vec::new();
                let mut count = 0usize;
                for cap in re.captures_iter(&subject) {
                    count += 1;
                    let match_items: Vec<PhpArrayItem> = (0..num_groups)
                        .map(|i| {
                            PhpArrayItem::Value(PhpValue::String(
                                cap.get(i).map(|m| m.as_str().to_string()).unwrap_or_default(),
                            ))
                        })
                        .collect();
                    matches.push(PhpArrayItem::Value(PhpValue::Array(match_items)));
                }
                if let Some(var) = captures_var {
                    self.variables.insert(var, PhpValue::Array(matches));
                }
                Ok(PhpValue::Int(count as i64))
            } else {
                // Default PREG_PATTERN_ORDER: [[full_match, ...], [group1, ...], ...]
                let mut groups: Vec<Vec<String>> = vec![Vec::new(); num_groups];
                for cap in re.captures_iter(&subject) {
                    for i in 0..num_groups {
                        groups[i].push(
                            cap.get(i)
                                .map(|m| m.as_str().to_string())
                                .unwrap_or_default(),
                        );
                    }
                }
                let count = groups.first().map(|g| g.len()).unwrap_or(0);
                if let Some(var) = captures_var {
                    let outer: Vec<PhpArrayItem> = groups
                        .into_iter()
                        .map(|g| {
                            PhpArrayItem::Value(PhpValue::Array(
                                g.into_iter()
                                    .map(|s| PhpArrayItem::Value(PhpValue::String(s)))
                                    .collect(),
                            ))
                        })
                        .collect();
                    self.variables.insert(var, PhpValue::Array(outer));
                }
                Ok(PhpValue::Int(count as i64))
            }
        } else {
            // preg_match: first match only
            match re.captures(&subject) {
                Some(caps) => {
                    if let Some(var) = captures_var {
                        let items: Vec<PhpArrayItem> = (0..caps.len())
                            .map(|i| {
                                PhpArrayItem::Value(PhpValue::String(
                                    caps.get(i)
                                        .map(|m| m.as_str().to_string())
                                        .unwrap_or_default(),
                                ))
                            })
                            .collect();
                        self.variables.insert(var, PhpValue::Array(items));
                    }
                    Ok(PhpValue::Int(1))
                }
                None => {
                    if let Some(var) = captures_var {
                        self.variables.insert(var, PhpValue::Array(vec![]));
                    }
                    Ok(PhpValue::Int(0))
                }
            }
        }
    }

    /// Open a TLS client connection to host:port
    async fn connect_tls(
        &self,
        host: &str,
        port: u16,
    ) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
        let mut root_store = rustls::RootCertStore::empty();
        root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(config));
        let server_name = ServerName::try_from(host.to_string())
            .map_err(|e| anyhow!("Invalid TLS server name '{}': {}", host, e))?;
        let tcp = TcpStream::connect((host, port))
            .await
            .map_err(|e| anyhow!("TCP connect to {}:{} failed: {}", host, port, e))?;
        let tls = connector
            .connect(server_name, tcp)
            .await
            .map_err(|e| anyhow!("TLS handshake with {}:{} failed: {}", host, port, e))?;
        Ok(tls)
    }

    fn take_side_effect_output(&mut self) -> Option<String> {
        self.side_effect_output.take()
    }

    async fn include_file(
        &mut self,
        target: &str,
        once: bool,
    ) -> Result<(String, Option<PhpValue>)> {
        debug!("[{}] PHP include: {} (once={})", self.request_domain, target, once);
        let path = self.resolve_local_path(target)?;
        let canonical = path
            .canonicalize()
            .map_err(|e| {
                self.log_error(&format!("Cannot resolve include path: {} → {}", target, e));
                anyhow!("Cannot resolve include path")
            })?;

        if once && self.included_files.contains(&canonical) {
            return Ok((String::new(), None));
        }

        let content = fs::read_to_string(&canonical)
            .await
            .map_err(|_| anyhow!("Cannot read include file"))?;

        let previous_source = self.source_code.clone();
        let previous_template = self.current_template_path.clone();
        // Save and clear return state so the included file's return is captured
        let saved_script_returned = self.script_returned.take();
        let saved_return_value = self.script_return_value.take();
        let saved_body_override = self.response_body_override.take();

        self.current_template_path = Some(canonical.clone());
        self.included_files.insert(canonical);

        let mut output = String::new();
        let result = self.process_php_code(&content, &mut output).await;

        // Capture included file's return value before restoring outer state
        let return_value = self.script_return_value.take();
        debug!("[{}] PHP include done: {} => output_len={}, return={:?}", self.request_domain,
            target, output.len(),
            return_value.as_ref().map(|v| match v {
                PhpValue::Array(items) => format!("Array({} items)", items.len()),
                other => format!("{:?}", other),
            }).unwrap_or_else(|| "None".to_string()));

        // Restore outer context's return state
        self.script_returned = saved_script_returned;
        self.script_return_value = saved_return_value;
        self.response_body_override = saved_body_override;
        self.source_code = previous_source;
        self.current_template_path = previous_template;

        // Propagate errors (including PhpExit) after restoring state.
        // Preserve any echo output from the included file so the caller can recover it
        // (execute_chain checks accumulated_output when PhpExit is caught).
        if result.is_err() && !output.is_empty() {
            self.accumulated_output.push_str(&output);
        }
        result?;

        Ok((output, return_value))
    }
}

//// Build a Rust regex string from a PHP regex literal (with delimiter + flags).
/// Converts PHP flags i/s/m/x to Rust inline group (?ism).
fn build_regex_with_flags(input: &str) -> String {
    let s = input.trim();
    if s.len() < 2 {
        return s.to_string();
    }
    let delim = s.as_bytes()[0];
    if !matches!(delim, b'/' | b'#' | b'~' | b'|' | b'@' | b'!') {
        return s.to_string();
    }
    if let Some(end_pos) = s[1..].rfind(delim as char) {
        let pattern = &s[1..end_pos + 1];
        let flags: String = s[end_pos + 2..]
            .chars()
            .filter(|c| matches!(c, 'i' | 's' | 'm' | 'x'))
            .collect();
        if flags.is_empty() {
            pattern.to_string()
        } else {
            format!("(?{}){}", flags, pattern)
        }
    } else {
        s.to_string()
    }
}

/// Basic HTML entity decoding for common named + numeric entities.
fn html_entity_decode_basic(s: &str) -> String {
    let mut out = s.to_string();
    // Named entities
    out = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&nbsp;", "\u{00A0}")
        .replace("&mdash;", "—")
        .replace("&ndash;", "–")
        .replace("&laquo;", "«")
        .replace("&raquo;", "»")
        .replace("&hellip;", "…")
        .replace("&copy;", "©")
        .replace("&reg;", "®")
        .replace("&trade;", "™");
    // Numeric decimal: &#123;
    if let Ok(re) = Regex::new(r"&#(\d+);") {
        out = re
            .replace_all(&out, |caps: &regex::Captures| {
                caps[1]
                    .parse::<u32>()
                    .ok()
                    .and_then(char::from_u32)
                    .map(|c| c.to_string())
                    .unwrap_or_default()
            })
            .to_string();
    }
    // Numeric hex: &#x1F;
    if let Ok(re) = Regex::new(r"(?i)&#x([0-9a-f]+);") {
        out = re
            .replace_all(&out, |caps: &regex::Captures| {
                u32::from_str_radix(&caps[1], 16)
                    .ok()
                    .and_then(char::from_u32)
                    .map(|c| c.to_string())
                    .unwrap_or_default()
            })
            .to_string();
    }
    out
}

// Strip PHP regex delimiters: /pattern/flags -> (pattern, flags)
#[allow(dead_code)]
fn strip_php_regex_delimiters(input: &str) -> (String, String) {
    let s = input.trim();
    if s.len() < 2 {
        return (s.to_string(), String::new());
    }
    let delim = s.as_bytes()[0];
    // Common PHP delimiters: / # ~ |
    if !matches!(delim, b'/' | b'#' | b'~' | b'|' | b'@' | b'!') {
        return (s.to_string(), String::new());
    }
    if let Some(end_pos) = s[1..].rfind(delim as char) {
        let pattern = s[1..end_pos + 1].to_string();
        let flags = s[end_pos + 2..].to_string();
        (pattern, flags)
    } else {
        (s.to_string(), String::new())
    }
}

/// Convert PhpValue to JSON string
fn php_value_to_json(val: &PhpValue) -> String {
    match val {
        PhpValue::Null => "null".to_string(),
        PhpValue::Bool(b) => {
            if *b {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        PhpValue::Int(n) => n.to_string(),
        PhpValue::Float(f) => f.to_string(),
        PhpValue::String(s) => serde_json::to_string(s).unwrap_or_else(|_| format!("\"{}\"", s)),
        PhpValue::Array(items) => {
            // Determine if this is an associative array or a list
            let is_assoc = items
                .iter()
                .any(|i| matches!(i, PhpArrayItem::KeyValue(_, _)));
            if is_assoc {
                let pairs: Vec<String> = items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| match item {
                        PhpArrayItem::KeyValue(k, v) => format!(
                            "{}:{}",
                            serde_json::to_string(k).unwrap_or_default(),
                            php_value_to_json(v)
                        ),
                        PhpArrayItem::Value(v) => format!("\"{}\":{}", i, php_value_to_json(v)),
                    })
                    .collect();
                format!("{{{}}}", pairs.join(","))
            } else {
                let values: Vec<String> = items
                    .iter()
                    .map(|item| match item {
                        PhpArrayItem::KeyValue(_, v) => php_value_to_json(v),
                        PhpArrayItem::Value(v) => php_value_to_json(v),
                    })
                    .collect();
                format!("[{}]", values.join(","))
            }
        }
        PhpValue::Resource(_) => "null".to_string(),
        PhpValue::Object(obj) => {
            // Serialise object properties as a JSON object
            let pairs: Vec<String> = obj
                .props
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(k).unwrap_or_default(),
                        php_value_to_json(v)
                    )
                })
                .collect();
            format!("{{{}}}", pairs.join(","))
        }
    }
}

/// Convert serde_json::Value to PhpValue
fn json_to_php_value(json: &serde_json::Value, assoc: bool) -> PhpValue {
    match json {
        serde_json::Value::Null => PhpValue::Null,
        serde_json::Value::Bool(b) => PhpValue::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                PhpValue::Int(i)
            } else if let Some(f) = n.as_f64() {
                PhpValue::Float(f)
            } else {
                PhpValue::String(n.to_string())
            }
        }
        serde_json::Value::String(s) => PhpValue::String(s.clone()),
        serde_json::Value::Array(arr) => {
            let items: Vec<PhpArrayItem> = arr
                .iter()
                .map(|v| PhpArrayItem::Value(json_to_php_value(v, assoc)))
                .collect();
            PhpValue::Array(items)
        }
        serde_json::Value::Object(obj) => {
            let items: Vec<PhpArrayItem> = obj
                .iter()
                .map(|(k, v)| PhpArrayItem::KeyValue(k.clone(), json_to_php_value(v, assoc)))
                .collect();
            PhpValue::Array(items)
        }
    }
}

/// Simple MD5 hash (pure Rust, no external crate)
fn md5_hash(data: &[u8]) -> u128 {
    // This is a placeholder — for proper md5, use the md5 crate
    // For now return a hash derived from the data
    let mut hash: u128 = 0;
    for (i, &byte) in data.iter().enumerate() {
        hash = hash
            .wrapping_mul(31)
            .wrapping_add(byte as u128)
            .wrapping_add(i as u128);
    }
    hash
}

fn parse_header_block(headers: &str) -> Vec<(String, String)> {
    headers
        .split(|c| c == '\n' || c == '\r')
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            line.split_once(':')
                .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        })
        .collect()
}

pub struct PhpExecution {
    pub body: String,
    pub status: u16,
    pub headers: HashMap<String, String>,
    /// true when the script called exit/die (request fully handled)
    pub exited: bool,
    /// Return value from the script: Some(true) = pass through, Some(false) = handled, None = fell off end
    pub returned: Option<bool>,
}

#[derive(Clone, Debug)]
pub enum PhpValue {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Array(Vec<PhpArrayItem>),
    Null,
    /// Opaque resource handle (e.g. socket stream)
    Resource(u32),
    /// PHP object instance
    Object(Box<PhpObject>),
}

#[derive(Clone, Debug)]
pub enum PhpArrayItem {
    KeyValue(String, PhpValue),
    Value(PhpValue),
}

/// Sentinel error type for PHP exit/die
#[derive(Debug)]
pub struct PhpExit {
    pub code: i32,
}

impl std::fmt::Display for PhpExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exit({})", self.code)
    }
}

impl PhpValue {
    pub fn as_string(&self) -> String {
        match self {
            PhpValue::String(value) => value.clone(),
            PhpValue::Int(n) => n.to_string(),
            PhpValue::Float(f) => {
                if f.fract() == 0.0 {
                    format!("{:.0}", f)
                } else {
                    f.to_string()
                }
            }
            PhpValue::Bool(b) => {
                if *b {
                    "1".to_string()
                } else {
                    String::new()
                }
            }
            PhpValue::Array(_) => "Array".to_string(),
            PhpValue::Null => String::new(),
            PhpValue::Resource(id) => format!("Resource id #{}", id),
            PhpValue::Object(obj) => {
                // Special handling for SimpleXMLElement string conversion
                if obj.class_name == "SimpleXMLElement" {
                    xml_parser::get_xml_text(self)
                } else {
                    format!("Object({})", obj.class_name)
                }
            }
        }
    }

    /// Returns true if this value is integer-typed (Int or Bool — PHP promotes bools to int in arithmetic).
    pub fn is_int_like(&self) -> bool {
        matches!(self, PhpValue::Int(_) | PhpValue::Bool(_))
    }

    pub fn as_int(&self) -> i64 {
        match self {
            PhpValue::Int(n) => *n,
            PhpValue::Float(f) => *f as i64,
            PhpValue::Bool(b) => {
                if *b {
                    1
                } else {
                    0
                }
            }
            PhpValue::String(s) => s.parse::<i64>().unwrap_or(0),
            PhpValue::Null => 0,
            PhpValue::Array(items) => {
                if items.is_empty() {
                    0
                } else {
                    1
                }
            }
            PhpValue::Resource(id) => *id as i64,
            PhpValue::Object(_) => 1,
        }
    }

    pub fn as_float(&self) -> f64 {
        match self {
            PhpValue::Float(f) => *f,
            PhpValue::Int(n) => *n as f64,
            PhpValue::Bool(b) => {
                if *b {
                    1.0
                } else {
                    0.0
                }
            }
            PhpValue::String(s) => s.parse::<f64>().unwrap_or(0.0),
            PhpValue::Null => 0.0,
            PhpValue::Array(items) => {
                if items.is_empty() {
                    0.0
                } else {
                    1.0
                }
            }
            PhpValue::Resource(id) => *id as f64,
            PhpValue::Object(_) => 1.0,
        }
    }

    pub fn is_truthy(&self) -> bool {
        match self {
            PhpValue::Bool(b) => *b,
            PhpValue::Int(n) => *n != 0,
            PhpValue::Float(f) => *f != 0.0,
            PhpValue::String(s) => !s.is_empty() && s != "0",
            PhpValue::Array(items) => !items.is_empty(),
            PhpValue::Null => false,
            PhpValue::Resource(_) => true, // resources are always truthy
            PhpValue::Object(_) => true,   // objects are always truthy
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, PhpValue::Null)
    }

    pub fn is_empty_value(&self) -> bool {
        match self {
            PhpValue::Null => true,
            PhpValue::Bool(b) => !b,
            PhpValue::Int(n) => *n == 0,
            PhpValue::Float(f) => *f == 0.0,
            PhpValue::String(s) => s.is_empty() || s == "0",
            PhpValue::Array(items) => items.is_empty(),
            PhpValue::Resource(_) => false, // resources are never empty
            PhpValue::Object(_) => false,   // objects are never empty
        }
    }

    /// PHP loose equality (==)
    pub fn loose_eq(&self, other: &PhpValue) -> bool {
        match (self, other) {
            (PhpValue::Null, PhpValue::Null) => true,
            (PhpValue::Null, PhpValue::Bool(false)) | (PhpValue::Bool(false), PhpValue::Null) => {
                true
            }
            (PhpValue::Null, PhpValue::String(s)) | (PhpValue::String(s), PhpValue::Null) => {
                s.is_empty()
            }
            (PhpValue::Null, PhpValue::Int(0)) | (PhpValue::Int(0), PhpValue::Null) => true,
            (PhpValue::Null, _) | (_, PhpValue::Null) => false,
            (PhpValue::Bool(a), _) => *a == other.is_truthy(),
            (_, PhpValue::Bool(b)) => self.is_truthy() == *b,
            (PhpValue::Int(a), PhpValue::Int(b)) => a == b,
            (PhpValue::Float(a), PhpValue::Float(b)) => a == b,
            (PhpValue::Int(a), PhpValue::Float(b)) | (PhpValue::Float(b), PhpValue::Int(a)) => {
                (*a as f64) == *b
            }
            (PhpValue::String(a), PhpValue::String(b)) => {
                // If both parse as numbers, compare numerically
                if let (Ok(an), Ok(bn)) = (a.parse::<f64>(), b.parse::<f64>()) {
                    an == bn
                } else {
                    a == b
                }
            }
            (PhpValue::String(s), PhpValue::Int(n)) | (PhpValue::Int(n), PhpValue::String(s)) => {
                s.parse::<i64>().map_or(false, |sn| sn == *n)
            }
            // Objects: two different object values are not loosely equal (no identity tracking)
            (PhpValue::Object(a), PhpValue::Object(b)) => {
                a.class_name == b.class_name && a.props.len() == b.props.len()
            }
            _ => self.as_string() == other.as_string(),
        }
    }

    /// PHP strict equality (===)
    pub fn strict_eq(&self, other: &PhpValue) -> bool {
        match (self, other) {
            (PhpValue::Null, PhpValue::Null) => true,
            (PhpValue::Bool(a), PhpValue::Bool(b)) => a == b,
            (PhpValue::Int(a), PhpValue::Int(b)) => a == b,
            (PhpValue::Float(a), PhpValue::Float(b)) => a == b,
            (PhpValue::String(a), PhpValue::String(b)) => a == b,
            (PhpValue::Object(a), PhpValue::Object(b)) => {
                // No object identity tracking at MVP: same class + same props
                std::ptr::eq(a.as_ref(), b.as_ref())
            }
            _ => false,
        }
    }

    /// Array item count or string length for count()
    pub fn count(&self) -> usize {
        match self {
            PhpValue::Array(items) => items.len(),
            PhpValue::String(s) => s.len(),
            _ => 0,
        }
    }

    /// Get array item by key
    #[allow(dead_code)]
    pub fn array_get(&self, key: &str) -> PhpValue {
        if let PhpValue::Array(items) = self {
            // Try numeric index first
            if let Ok(idx) = key.parse::<usize>() {
                if let Some(item) = items.get(idx) {
                    return match item {
                        PhpArrayItem::KeyValue(_, v) => v.clone(),
                        PhpArrayItem::Value(v) => v.clone(),
                    };
                }
            }
            for item in items {
                if let PhpArrayItem::KeyValue(k, v) = item {
                    if k == key {
                        return v.clone();
                    }
                }
            }
        }
        PhpValue::Null
    }

    pub fn dump(&self) -> String {
        match self {
            PhpValue::String(value) => format!("string({}) \"{}\"", value.len(), value),
            PhpValue::Int(n) => format!("int({})", n),
            PhpValue::Float(f) => format!("float({})", f),
            PhpValue::Bool(b) => format!("bool({})", if *b { "true" } else { "false" }),
            PhpValue::Array(items) => {
                let mut out = String::new();
                out.push_str(&format!("array({}) {{\n", items.len()));
                for (index, item) in items.iter().enumerate() {
                    match item {
                        PhpArrayItem::KeyValue(key, value) => {
                            out.push_str(&format!("  [\"{}\"]=>\n    {}\n", key, value.dump()));
                        }
                        PhpArrayItem::Value(value) => {
                            out.push_str(&format!("  [{}]=>\n    {}\n", index, value.dump()));
                        }
                    }
                }
                out.push('}');
                out
            }
            PhpValue::Null => "NULL".to_string(),
            PhpValue::Resource(id) => format!("resource({})", id),
            PhpValue::Object(obj) => {
                let mut out = format!("object({})#{} ({}) {{\n", obj.class_name, 1, obj.props.len());
                for (key, val) in &obj.props {
                    out.push_str(&format!("  [\"{}\"]=>\n    {}\n", key, val.dump()));
                }
                out.push('}');
                out
            }
        }
    }
}

impl From<String> for PhpValue {
    fn from(value: String) -> Self {
        PhpValue::String(value)
    }
}

impl From<bool> for PhpValue {
    fn from(value: bool) -> Self {
        PhpValue::Bool(value)
    }
}

impl From<i64> for PhpValue {
    fn from(value: i64) -> Self {
        PhpValue::Int(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;
    use tokio::fs::write;

    #[tokio::test]
    async fn test_ast_php_processor_creation() {
        let processor = AstPhpProcessor::new().unwrap();
        // Ensure the parser is initialized
        assert!(processor.parser.language().is_some());
    }

    #[tokio::test]
    async fn test_ast_php_execution() {
        let mut processor = AstPhpProcessor::new().unwrap();
        let get_params = HashMap::new();
        let post_params = HashMap::new();
        let server_vars = HashMap::new();

        let php_code = r#"<html><body><?php echo "Hello from AST PHP!"; ?></body></html>"#;

        let temp_dir = TempDir::new().unwrap();
        let template_path = temp_dir.path().join("_index.php");
        write(&template_path, php_code).await.unwrap();

        let result = processor
            .execute_php(
                php_code,
                &get_params,
                &post_params,
                &server_vars,
                &template_path,
                temp_dir.path(),
            )
            .await
            .unwrap();
        assert!(result.body.contains("Hello from AST PHP!"));
    }

    #[tokio::test]
    async fn test_ast_php_inline_html() {
        let mut processor = AstPhpProcessor::new().unwrap();
        let get_params = HashMap::new();
        let post_params = HashMap::new();
        let server_vars = HashMap::new();

        let php_code = r#"<html><body>Welcome to <?php echo "My Site"; ?></body></html>"#;

        let temp_dir = TempDir::new().unwrap();
        let template_path = temp_dir.path().join("_index.php");
        write(&template_path, php_code).await.unwrap();

        let result = processor
            .execute_php(
                php_code,
                &get_params,
                &post_params,
                &server_vars,
                &template_path,
                temp_dir.path(),
            )
            .await
            .unwrap();
        assert!(result.body.contains("Welcome to My Site"));
    }

    #[tokio::test]
    async fn test_ast_php_variable_echo() {
        let mut processor = AstPhpProcessor::new().unwrap();
        let mut get_params = HashMap::new();
        get_params.insert("user".to_string(), "Alice".to_string());
        let post_params = HashMap::new();
        let mut server_vars = HashMap::new();
        server_vars.insert("SERVER_NAME".to_string(), "localhost".to_string());

        let php_code = r#"<?php $name = $_GET['user']; echo "Hello, " . $name; ?>"#;

        let temp_dir = TempDir::new().unwrap();
        let template_path = temp_dir.path().join("_index.php");
        write(&template_path, php_code).await.unwrap();

        let result = processor
            .execute_php(
                php_code,
                &get_params,
                &post_params,
                &server_vars,
                &template_path,
                temp_dir.path(),
            )
            .await
            .unwrap();
        assert!(result.body.contains("Hello, Alice"));
    }

    #[tokio::test]
    async fn test_multiple_error_log_calls() {
        use std::sync::{Arc, Mutex};

        let mut processor = AstPhpProcessor::new().unwrap();
        let get_params = HashMap::new();
        let post_params = HashMap::new();
        let mut server_vars = HashMap::new();
        server_vars.insert(
            "HTTP_REFERER".to_string(),
            "https://example.com".to_string(),
        );

        let php_code = r#"<?php
error_log("referer: {$_SERVER['HTTP_REFERER']}");
trigger_error("asdfds");
error_log('fsdfsd');
echo 'hi';
"#;

        let temp_dir = TempDir::new().unwrap();
        let template_path = temp_dir.path().join("_index.php");
        write(&template_path, php_code).await.unwrap();

        let logged = Arc::new(Mutex::new(Vec::<String>::new()));
        let logged_clone = logged.clone();
        let handler: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(move |msg: &str| {
            logged_clone.lock().unwrap().push(msg.to_string());
        });

        let result = processor
            .execute_php_with_handler(
                php_code,
                &get_params,
                &post_params,
                &server_vars,
                &template_path,
                temp_dir.path(),
                Some(handler),
            )
            .await
            .unwrap();

        let messages = logged.lock().unwrap();
        eprintln!("Logged messages: {:?}", messages);
        eprintln!("Output body: {:?}", result.body);
        eprintln!("Returned: {:?}", result.returned);
        eprintln!("Exited: {:?}", result.exited);
        assert_eq!(
            messages.len(),
            3,
            "Expected 3 log messages, got {}: {:?}",
            messages.len(),
            *messages
        );
        assert!(messages[0].contains("referer"));
        assert!(messages[1].contains("asdfds"));
        assert!(messages[2].contains("fsdfsd"));
        assert!(result.body.contains("hi"), "Body was: {:?}", result.body);
    }

    #[tokio::test]
    async fn test_ast_cookie_superglobal() {
        let mut processor = AstPhpProcessor::new().unwrap();
        let get_params = HashMap::new();
        let post_params = HashMap::new();
        let mut server_vars = HashMap::new();
        server_vars.insert(
            "HTTP_COOKIE".to_string(),
            "foo=bar%20baz; theme=dark".to_string(),
        );

        let php_code = r#"<?php echo $_COOKIE['foo'] . ':' . $_COOKIE['theme']; ?>"#;
        let temp_dir = TempDir::new().unwrap();
        let template_path = temp_dir.path().join("_index.php");
        write(&template_path, php_code).await.unwrap();

        let result = processor
            .execute_php(
                php_code,
                &get_params,
                &post_params,
                &server_vars,
                &template_path,
                temp_dir.path(),
            )
            .await
            .unwrap();
        assert_eq!(result.body, "bar baz:dark");
    }

    #[tokio::test]
    async fn test_ruph_session_persists_session_superglobal() {
        let session_id = format!("test_{}_ast_session", std::process::id());
        let session_path = AstPhpProcessor::session_file_path(&session_id);
        let _ = std::fs::remove_file(&session_path);

        let get_params = HashMap::new();
        let post_params = HashMap::new();
        let server_vars = HashMap::new();
        let temp_dir = TempDir::new().unwrap();
        let template_path = temp_dir.path().join("_index.php");

        let php_code = format!(
            r#"<?php ruph_session_start("{}"); $_SESSION['name'] = 'Alice'; echo ruph_session_id(); ?>"#,
            session_id
        );
        write(&template_path, &php_code).await.unwrap();

        let mut writer = AstPhpProcessor::new().unwrap();
        let result = writer
            .execute_php(
                &php_code,
                &get_params,
                &post_params,
                &server_vars,
                &template_path,
                temp_dir.path(),
            )
            .await
            .unwrap();
        assert_eq!(result.body, session_id);
        assert!(result
            .headers
            .get("set-cookie")
            .unwrap()
            .contains("RUPHSESSID="));

        let php_code = format!(
            r#"<?php ruph_session_start("{}"); echo $_SESSION['name']; ?>"#,
            session_id
        );
        write(&template_path, &php_code).await.unwrap();

        let mut reader = AstPhpProcessor::new().unwrap();
        let result = reader
            .execute_php(
                &php_code,
                &get_params,
                &post_params,
                &server_vars,
                &template_path,
                temp_dir.path(),
            )
            .await
            .unwrap();
        assert_eq!(result.body, "Alice");

        let _ = std::fs::remove_file(&session_path);
    }

    #[tokio::test]
    async fn test_ast_multiple_set_cookie_headers_are_preserved() {
        let mut processor = AstPhpProcessor::new().unwrap();
        let get_params = HashMap::new();
        let post_params = HashMap::new();
        let server_vars = HashMap::new();

        let php_code = r#"<?php setcookie('a', '1'); setcookie('b', '2'); ?>"#;
        let temp_dir = TempDir::new().unwrap();
        let template_path = temp_dir.path().join("_index.php");
        write(&template_path, php_code).await.unwrap();

        let result = processor
            .execute_php(
                php_code,
                &get_params,
                &post_params,
                &server_vars,
                &template_path,
                temp_dir.path(),
            )
            .await
            .unwrap();
        assert_eq!(result.headers.get("set-cookie"), Some(&"a=1".to_string()));
        assert_eq!(result.headers.get("set-cookie-2"), Some(&"b=2".to_string()));
    }
}
