# ruph.ini Configuration Reference

## File Discovery

ruph searches for `ruph.ini` in this order (first match wins):

| Priority | Location | Notes |
|----------|----------|-------|
| 1 | `<docroot>/ruph.ini` | Only if a docroot is provided via CLI |
| 2 | `./ruph.ini` | Current working directory |
| 3 | `~/.ruph/ruph.ini` | Per-user config |
| 4 | `/etc/ruph/ruph.ini` | System-wide |

Override with `--config <path>` to use a specific file.

On startup, ruph prints which config it loaded:
```
config: /var/www/live/ruph_root/ruph.ini
```

## Sections

### [server] — Global Settings

| Key | Default | Description |
|-----|---------|-------------|
| `bind` | `0.0.0.0:8082` | Listen address (only if `[server.https]` is absent) |
| `tls` | `false` | Enable TLS (only if `[server.https]` is absent) |
| `log_level` | `info` | Log level: `error`, `warn`, `info`, `debug`, `trace` |
| `log_console` | `false` | Also log to console (in addition to file) |
| `access_log` | — | Default access log file path (alias: `logs`) |
| `error_log` | — | Default error log file path (PHP errors, AST warnings) |
| `index_files` | `_index.php,index.html,index.htm` | Comma-separated index file names |
| `docroot` | — | Default document root |
| `status_page` | — | Path for status endpoint (e.g. `/status`) |
| `rate_window` | `2` | Sliding window in seconds for per-IP rate limiting |
| `log_full` | — | Path to SQLite DB for full request logging |

### [server.https] — HTTPS Listener

| Key | Default | Description |
|-----|---------|-------------|
| `bind` | — | HTTPS bind address (e.g. `0.0.0.0:443`) |
| `tls` | `true` | TLS enabled (implied by HTTPS) |

### [server.http] — HTTP Listener

| Key | Default | Description |
|-----|---------|-------------|
| `bind` | — | HTTP bind address (e.g. `0.0.0.0:80`) |

### [php.*] or [php] — PHP Processing

| Key | Default | Description |
|-----|---------|-------------|
| `processor` | `auto` | Processing mode (see below) |
| `binary` | auto-detected | Path to `php-cgi` binary (for `cgi`/`auto` modes) |

**Processor modes:**

| Mode | Value | Description |
|------|-------|-------------|
| Auto | `auto` | AST first, then embedded, then CGI fallback |
| AST | `ast` | Built-in AST interpreter only, no fallback |
| CGI | `cgi`, `external`, `php` | External php-cgi first, then AST/embedded fallback |
| Embedded | `embedded`, `regex` | Regex-based processor only |

### [trailhead] — Remote Log Ingestion

Ships full request records as NDJSON to the Trailhead API. Batches up to 400 events, flushing every 5 seconds. Requires both `api_url` and `api_key` to enable.

| Key | Default | Description |
|-----|---------|-------------|
| `api_url` | — | Trailhead API endpoint URL |
| `api_key` | — | API key for authentication |
| `default_owner` | — | Fallback owner for domains without `trailhead_owner` |

When a domain resolves to a `trailhead_owner` (per-domain or default), file-based access logging is skipped for that domain. Error logs always go to file.

### [ssl] — TLS Certificate Directory

| Key | Default | Description |
|-----|---------|-------------|
| `dir` | `~/.ruph/ssl` | Directory containing per-domain TLS certificates |

## Virtual Host Sections

### [http.*] — Default HTTP Docroot

All HTTP requests that don't match a specific domain use this docroot.

```ini
[http.*]
docroot = /var/www/live/ruph_root/ruph_http
```

### [https.*] — Default HTTPS Docroot

Fallback docroot for HTTPS requests with no matching domain.

```ini
[https.*]
docroot = /var/www/default
```

### [https.&lt;domain&gt;] — Per-Domain Virtual Host

Exact domain matching (case-insensitive). The domain portion is everything after `https.`.

```ini
[https.example.com]
docroot = /var/www/example.com
access_log = /var/log/ruph/example.log
error_log = /var/log/ruph/example_error.log
trailhead_owner = example
```

**Keys:**

| Key | Description |
|-----|-------------|
| `docroot` | Document root for this domain |
| `access_log` | Access log file for this domain (alias: `logs`) |
| `error_log` | Error log file for this domain |
| `trailhead_owner` | Trailhead log group owner; when set, skips file access_log |

### Prefix Matching

Section names without a dot in the domain part are treated as prefix matches. For example, `[https.www]` matches any host starting with `www` (e.g. `www.example.com`, `www.nyphp.org`).

```ini
;; Redirect all www.* to HTTPS without www
[https.www]
docroot = /var/www/live/ruph_root/ruph_http
```

### Comma-Separated Domains

Multiple domains can share the same docroot:

```ini
[https.junkometer.com,https.junkmeter.com]
docroot = /var/www/live/ruph_root/junkometer.com
```

## Full Example

```ini
;; Global settings
[server]
access_log = /var/www/live/ruph_logs/ruph.log
error_log = /var/www/live/ruph_logs/ruph_error.log
log_full = /var/www/live/ruph_logs/requests.db
index_files = _index.php,index.php,index.html

;; Remote log ingestion (optional — replaces file access_log for matched domains)
[trailhead]
api_url = https://your-api-id.execute-api.region.amazonaws.com/v1
api_key = your-api-key
default_owner = myserver

;; HTTPS on port 443
[server.https]
bind = 0.0.0.0:443
tls = true

;; HTTP on port 80 (for redirects)
[server.http]
bind = 0.0.0.0:80
tls = false

;; PHP: AST-first with CGI fallback
[php.*]
processor = auto
binary = /usr/local/bin/php-cgi

;; All HTTP requests → redirect docroot
[http.*]
docroot = /var/www/live/ruph_root/ruph_http

;; www.* over HTTPS → also redirect
[https.www]
docroot = /var/www/live/ruph_root/ruph_http

;; Per-domain HTTPS virtual hosts
[https.example.com]
docroot = /var/www/live/ruph_root/example.com
trailhead_owner = example

[https.nyphp.org]
docroot = /var/www/live/ruph_root/nyphp.org
trailhead_owner = nyphporg
```

## Backward Compatibility

ruph also supports older-style configuration with a flat `[http]` section using `docroot.<domain>` keys:

```ini
[http]
docroot = /var/www/html
docroot.example.com = /var/www/example.com
access_log.example.com = /var/log/example.log
```

This format is automatically parsed if no `[https.<domain>]` sections are found.
