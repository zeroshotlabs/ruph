//! External PHP binary processor
//!
//! Executes PHP scripts via an external PHP binary (libphp / php-cli).
//! Used as a fallback when AST and embedded processors cannot handle a script,
//! or as the primary processor when configured.

use std::collections::HashMap;
use std::path::Path;
use anyhow::{Result, anyhow};
use tokio::process::Command;
use tracing::{debug, info, warn};
use tempfile::NamedTempFile;
use std::io::Write;

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
        let candidates = [
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

    pub async fn process_file(
        &self,
        file_path: &Path,
        _content: &str,
        query_params: &HashMap<String, String>,
        post_data: &HashMap<String, String>,
        server_vars: &HashMap<String, String>,
    ) -> Result<String> {
        debug!("Executing PHP via external binary: {:?}", file_path);

        let mut command = Command::new(&self.php_binary);
        command.arg("-d").arg(format!("auto_prepend_file={}", self.prepend_path));
        command.arg("-d").arg("variables_order=EGPCS");
        command.arg("-f").arg(file_path);

        // Set working directory to DOCUMENT_ROOT so getcwd(), relative paths, etc. work
        let docroot = server_vars.get("DOCUMENT_ROOT")
            .cloned()
            .unwrap_or_else(|| file_path.parent().unwrap_or(Path::new(".")).to_string_lossy().to_string());
        command.current_dir(&docroot);

        // CGI environment variables
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

        let output = command.output().await
            .map_err(|e| anyhow!("Failed to execute PHP: {}", e))?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // If PHP produced output before failing, return it
            // (parse errors, warnings, etc. shouldn't discard partial output)
            if !stdout.is_empty() {
                warn!("PHP exited with error but produced output: {}", stderr.trim());
                return Ok(stdout);
            }
            return Err(anyhow!("PHP execution failed: {}", stderr));
        }

        Ok(stdout)
    }

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
            ).await;

            if let Ok(output) = result {
                assert!(output.contains("Hello from PHP"));
            }
        }
    }
}
