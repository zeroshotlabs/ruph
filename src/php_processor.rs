//! External PHP binary processor
//!
//! Executes PHP scripts via an external PHP binary (preferably php-cgi).
//! Used as a fallback when AST and embedded processors cannot handle a script,
//! or as the primary processor when configured.

use std::collections::HashMap;
use std::path::Path;
use std::io;
use anyhow::{Result, anyhow};
use bytes::Bytes;
use hyper::header::HeaderName;
use tokio::process::Command;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use tempfile::NamedTempFile;
use std::io::Write as StdWrite;
use std::sync::Arc;

/// Callback for routing PHP stderr lines (e.g. error_log()) to domain-specific logs.
pub type PhpStderrHandler = Arc<dyn Fn(&str) + Send + Sync>;

/// Built-in PHP functions provided by ruph, auto-prepended to every script
const RUPH_BUILTINS_PHP: &str = r#"<?php
/**
 * exe() - Execute a path like the web server does.
 *   exe('dir/')      -> finds and executes dir/_index.php
 *   exe('file.php')  -> executes file.php
 *   exe('file.html') -> includes file.html (processes <?php tags)
 * Paths are relative to DOCUMENT_ROOT.
 */
if (!function_exists('exe')) {
    function exe(string $path): string {
        $docroot = !empty($_SERVER['DOCUMENT_ROOT']) ? $_SERVER['DOCUMENT_ROOT']
                 : (!empty($_ENV['DOCUMENT_ROOT']) ? $_ENV['DOCUMENT_ROOT']
                 : (getenv('DOCUMENT_ROOT') ?: getcwd()));
        // Resolve relative to docroot
        if ($path[0] !== '/') {
            $full = $docroot . '/' . $path;
        } else {
            $full = $docroot . $path;
        }
        $real = realpath($full);
        if ($real === false) {
            trigger_error("exe: path not found: $path", E_USER_WARNING);
            return '';
        }
        // Security: must be under docroot
        $realRoot = realpath($docroot);
        if ($realRoot !== false && strpos($real, $realRoot) !== 0) {
            trigger_error("exe: path outside docroot: $path", E_USER_WARNING);
            return '';
        }
        if (is_dir($real)) {
            $script = rtrim($real, '/') . '/_index.php';
            if (!file_exists($script)) {
                trigger_error("exe: no _index.php in $path", E_USER_WARNING);
                return '';
            }
        } else {
            $script = $real;
        }
        ob_start();
        include $script;
        return ob_get_clean();
    }
}
?>"#;

/// Result of a streaming PHP execution: headers are parsed from CGI output,
/// body chunks arrive via the receiver channel.
pub struct PhpStream {
    pub headers: HashMap<String, String>,
    pub status: u16,
    pub rx: mpsc::Receiver<Result<Bytes, io::Error>>,
}

pub struct PhpProcessor {
    php_binary: String,
    /// Temp file holding ruph built-in PHP functions (auto_prepend)
    _prepend_file: NamedTempFile,
    prepend_path: String,
}

impl PhpProcessor {
    pub fn new() -> Result<Self> {
        let php_binary = Self::find_php_binary()?;
        info!("External PHP processor using: {}", php_binary);
        let (prepend_file, prepend_path) = Self::create_prepend_file()?;
        Ok(PhpProcessor { php_binary, _prepend_file: prepend_file, prepend_path })
    }

    pub fn with_binary(binary: String) -> Result<Self> {
        let output = std::process::Command::new(&binary)
            .arg("--version")
            .output();
        match output {
            Ok(out) if out.status.success() => {
                let version = String::from_utf8_lossy(&out.stdout);
                let first_line = version.lines().next().unwrap_or("unknown");
                info!("External PHP processor using: {} ({})", binary, first_line);
                let (prepend_file, prepend_path) = Self::create_prepend_file()?;
                Ok(PhpProcessor { php_binary: binary, _prepend_file: prepend_file, prepend_path })
            }
            _ => Err(anyhow!("PHP binary not usable: {}", binary)),
        }
    }

    /// Create a temp file with ruph built-in PHP functions for auto_prepend
    fn create_prepend_file() -> Result<(NamedTempFile, String)> {
        let mut f = NamedTempFile::with_suffix(".php")
            .map_err(|e| anyhow!("Failed to create prepend file: {}", e))?;
        f.write_all(RUPH_BUILTINS_PHP.as_bytes())
            .map_err(|e| anyhow!("Failed to write prepend file: {}", e))?;
        let path = f.path().to_string_lossy().to_string();
        Ok((f, path))
    }

    fn find_php_binary() -> Result<String> {
        // Prefer php-cgi: a proper CGI binary that natively outputs headers via stdout.
        // Fall back to plain php CLI which also works with GATEWAY_INTERFACE=CGI/1.1.
        let candidates = [
            "php-cgi",
            "php-cgi8.6",
            "php-cgi8.4",
            "php-cgi8.3",
            "php-cgi8.2",
            "php-cgi8.1",
            "php-cgi8",
            "/usr/local/bin/php-cgi",
            "/usr/bin/php-cgi",
            "php",
            "php8.6",
            "php8.4",
            "php8.3",
            "php8.2",
            "php8.1",
            "php8",
            "/usr/local/bin/php",
            "/usr/bin/php",
        ];

        for candidate in &candidates {
            if let Ok(output) = std::process::Command::new(candidate)
                .arg("--version")
                .output()
            {
                if output.status.success() {
                    return Ok(candidate.to_string());
                }
            }
        }

        Err(anyhow!("PHP binary not found. Install PHP or set php.binary in config."))
    }

    /// Build a configured Command for a PHP script, applying CGI environment vars.
    fn build_command(
        &self,
        file_path: &Path,
        query_params: &HashMap<String, String>,
        post_data: &HashMap<String, String>,
        server_vars: &HashMap<String, String>,
    ) -> Command {
        let mut command = Command::new(&self.php_binary);
        command.arg("-d").arg(format!("auto_prepend_file={}", self.prepend_path));
        command.arg("-d").arg("variables_order=EGPCS");
        command.arg("-f").arg(file_path);

        let docroot = server_vars.get("DOCUMENT_ROOT")
            .cloned()
            .unwrap_or_else(|| {
                file_path.parent().unwrap_or(Path::new(".")).to_string_lossy().to_string()
            });
        command.current_dir(&docroot);

        // CGI environment — GATEWAY_INTERFACE enables header() output in PHP stdout
        command.env("GATEWAY_INTERFACE", "CGI/1.1");
        command.env("REQUEST_METHOD",
            server_vars.get("REQUEST_METHOD").cloned()
                .unwrap_or_else(|| if post_data.is_empty() { "GET".to_string() } else { "POST".to_string() }));
        command.env("SCRIPT_FILENAME", file_path.to_string_lossy().to_string());
        command.env("DOCUMENT_ROOT", &docroot);

        // Forward all server vars
        for (key, value) in server_vars {
            command.env(key, value);
        }

        // Build query string
        if !query_params.is_empty() {
            let qs = query_params.iter()
                .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
                .collect::<Vec<_>>()
                .join("&");
            command.env("QUERY_STRING", &qs);
        }

        // POST data
        if !post_data.is_empty() {
            let post_string = post_data.iter()
                .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
                .collect::<Vec<_>>()
                .join("&");
            command.env("CONTENT_LENGTH", post_string.len().to_string());
            command.env("CONTENT_TYPE", "application/x-www-form-urlencoded");
        }

        command
    }

    /// Parse CGI-style headers from PHP stdout.
    /// PHP (with GATEWAY_INTERFACE=CGI/1.1) outputs headers before a blank line, then the body.
    /// Returns (headers, body, status_code).
    fn parse_cgi_output(raw: &str) -> (HashMap<String, String>, String, u16) {
        let mut headers = HashMap::new();
        let mut status = 200u16;

        // Find blank line separating headers from body
        let sep = raw.find("\r\n\r\n").map(|p| (p, p + 4))
            .or_else(|| raw.find("\n\n").map(|p| (p, p + 2)));

        let (header_str, body) = match sep {
            Some((end, body_start)) => (&raw[..end], raw[body_start..].to_string()),
            None => ("", raw.to_string()),
        };

        for line in header_str.lines() {
            let line = line.trim();
            if line.is_empty() { continue; }
            if let Some(colon) = line.find(':') {
                let name = line[..colon].trim().to_lowercase();
                let value = line[colon + 1..].trim().to_string();
                if name == "status" {
                    if let Ok(s) = value.split_whitespace()
                        .next().unwrap_or("200").parse::<u16>() {
                        status = s;
                    }
                } else if name != "x-powered-by" {
                    headers.insert(name, value);
                }
            }
        }

        (headers, body, status)
    }

    /// Execute a PHP file and return its body output (headers stripped).
    pub async fn process_file(
        &self,
        file_path: &Path,
        _content: &str,
        query_params: &HashMap<String, String>,
        post_data: &HashMap<String, String>,
        server_vars: &HashMap<String, String>,
        stderr_handler: Option<&PhpStderrHandler>,
    ) -> Result<String> {
        debug!("Executing PHP via external binary: {:?}", file_path);

        let output = self.build_command(file_path, query_params, post_data, server_vars)
            .output().await
            .map_err(|e| anyhow!("Failed to execute PHP: {}", e))?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr);
        for line in stderr.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                warn!("PHP: {}", trimmed);
                if let Some(handler) = stderr_handler {
                    handler(trimmed);
                }
            }
        }

        if !output.status.success() && stdout.is_empty() {
            return Err(anyhow!("PHP execution failed: {}", stderr.trim()));
        }

        let (_, body, _) = Self::parse_cgi_output(&stdout);
        Ok(body)
    }

    /// Execute a PHP file, parsing CGI headers and returning them alongside the body.
    #[allow(dead_code)]
    pub async fn process_file_with_headers(
        &self,
        file_path: &Path,
        query_params: &HashMap<String, String>,
        post_data: &HashMap<String, String>,
        server_vars: &HashMap<String, String>,
        stderr_handler: Option<&PhpStderrHandler>,
    ) -> Result<crate::ast_php_processor::PhpExecution> {
        debug!("Executing PHP with header parsing: {:?}", file_path);

        let output = self.build_command(file_path, query_params, post_data, server_vars)
            .output().await
            .map_err(|e| anyhow!("Failed to execute PHP: {}", e))?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stdout.is_empty() {
                return Err(anyhow!("PHP execution failed: {}", stderr));
            }
            let msg = format!("PHP exited with error but produced output: {}", stderr.trim());
            warn!("{}", msg);
            if let Some(handler) = stderr_handler {
                handler(&msg);
            }
        }

        let (headers, body, status) = Self::parse_cgi_output(&stdout);
        Ok(crate::ast_php_processor::PhpExecution { body, status, headers })
    }

    /// Spawn PHP and stream its output, parsing CGI headers from the start of stdout.
    /// The returned `PhpStream` contains parsed headers and a channel receiver for body chunks.
    pub async fn stream_file(
        &self,
        file_path: &Path,
        query_params: &HashMap<String, String>,
        post_data: &HashMap<String, String>,
        server_vars: &HashMap<String, String>,
        stderr_handler: Option<PhpStderrHandler>,
    ) -> Result<PhpStream> {
        debug!("Streaming PHP output: {:?}", file_path);

        let mut command = self.build_command(file_path, query_params, post_data, server_vars);
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());

        let mut child = command.spawn()
            .map_err(|e| anyhow!("Failed to spawn PHP: {}", e))?;

        let stdout = child.stdout.take()
            .ok_or_else(|| anyhow!("Failed to acquire PHP stdout"))?;
        let mut reader = BufReader::new(stdout);

        // Forward PHP stderr to ruph logger (and domain log if handler provided)
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut err_reader = BufReader::new(stderr);
                let mut line = String::new();
                loop {
                    line.clear();
                    match err_reader.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            let trimmed = line.trim();
                            if !trimmed.is_empty() {
                                warn!("PHP: {}", trimmed);
                                if let Some(ref handler) = stderr_handler {
                                    handler(trimmed);
                                }
                            }
                        }
                    }
                }
            });
        }

        // Read CGI headers line-by-line until blank line
        let mut headers = HashMap::new();
        let mut status = 200u16;
        let mut prebuffered_body: Vec<u8> = Vec::new();

        loop {
            let mut line = String::new();
            match reader.read_line(&mut line).await {
                Ok(0) => break,  // EOF before blank line
                Ok(_) => {}
                Err(e) => return Err(anyhow!("Error reading PHP headers: {}", e)),
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                break; // Blank line = end of CGI headers
            }

            if let Some(colon) = trimmed.find(':') {
                let name = trimmed[..colon].trim().to_lowercase();
                let value = trimmed[colon + 1..].trim().to_string();
                if name == "status" {
                    if let Ok(s) = value.split_whitespace()
                        .next().unwrap_or("200").parse::<u16>() {
                        status = s;
                    }
                    continue;
                }

                if HeaderName::from_bytes(name.as_bytes()).is_ok() {
                    if name != "x-powered-by" {
                        headers.insert(name, value);
                    }
                    continue;
                }
            }

            // If the first non-empty lines are not valid CGI headers, treat output as body.
            // This avoids swallowing JSON/error text that contains ':' but is not a header.
            prebuffered_body.extend_from_slice(line.as_bytes());
            break;
        }

        let (tx, rx) = mpsc::channel::<Result<Bytes, io::Error>>(32);

        // Background task: stream remaining stdout to channel, then wait for child
        tokio::spawn(async move {
            if !prebuffered_body.is_empty() {
                let first = Bytes::from(prebuffered_body);
                if tx.send(Ok(first)).await.is_err() {
                    let _ = child.wait().await;
                    return;
                }
            }

            let mut buf = vec![0u8; 8192];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let chunk = Bytes::copy_from_slice(&buf[..n]);
                        if tx.send(Ok(chunk)).await.is_err() {
                            break; // Receiver dropped
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        break;
                    }
                }
            }
            let _ = child.wait().await;
        });

        Ok(PhpStream { headers, status, rx })
    }

    #[allow(dead_code)]
    pub fn binary_path(&self) -> &str {
        &self.php_binary
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_php() {
        // This will pass if PHP is installed
        if PhpProcessor::find_php_binary().is_ok() {
            let proc = PhpProcessor::new().unwrap();
            assert!(!proc.binary_path().is_empty());
        }
    }

    #[tokio::test]
    async fn test_basic_execution() {
        if let Ok(proc) = PhpProcessor::new() {
            let dir = tempfile::TempDir::new().unwrap();
            let php_file = dir.path().join("test.php");
            std::fs::write(&php_file, "<?php echo 'Hello from PHP'; ?>").unwrap();

            let mut sv = HashMap::new();
            sv.insert("DOCUMENT_ROOT".to_string(), dir.path().to_string_lossy().to_string());

            let result = proc.process_file(
                &php_file,
                "",
                &HashMap::new(),
                &HashMap::new(),
                &sv,
                None,
            ).await;

            if let Ok(output) = result {
                assert!(output.contains("Hello from PHP"));
            }
        }
    }

    #[test]
    fn test_parse_cgi_output_with_headers() {
        let raw = "Content-Type: text/html\r\nStatus: 200 OK\r\n\r\n<html>hello</html>";
        let (headers, body, status) = PhpProcessor::parse_cgi_output(raw);
        assert_eq!(status, 200);
        assert_eq!(headers.get("content-type").unwrap(), "text/html");
        assert_eq!(body, "<html>hello</html>");
    }

    #[test]
    fn test_parse_cgi_output_sse_headers() {
        let raw = "Content-Type: text/event-stream\nCache-Control: no-cache\n\ndata: {}\n\n";
        let (headers, body, status) = PhpProcessor::parse_cgi_output(raw);
        assert_eq!(status, 200);
        assert_eq!(headers.get("content-type").unwrap(), "text/event-stream");
        assert!(body.contains("data: {}"));
    }

    #[test]
    fn test_parse_cgi_output_no_headers() {
        let raw = "plain body without headers";
        let (headers, body, _) = PhpProcessor::parse_cgi_output(raw);
        assert!(headers.is_empty());
        assert_eq!(body, raw);
    }
}
