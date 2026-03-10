# ruph.ini Configuration

## Sections

### [server] — global settings

```ini
access_log = /path/to/default.log  ; default access log for all domains
error_log = /path/to/error.log     ; default error log (PHP errors, AST warnings)
index_files = _index.php,index.php,index.html
status_page = /status              ; URL path for status dashboard (omit to disable)
rate_window = 2                    ; IP rate-limit sliding window in seconds
log_full = /path/to/requests.db    ; full request logging to SQLite (omit to disable)
log_level = info
log_console = false
```

### [trailhead] — remote log ingestion

Ships full request records (NDJSON) to the Trailhead API. Batches up to 400
events and flushes every 5 seconds. When a domain has `trailhead_owner` set,
file-based access logging is skipped — all request data goes to Trailhead instead.

```ini
[trailhead]
api_url = https://your-api-id.execute-api.region.amazonaws.com/v1
api_key = your-api-key
default_owner = myserver   ; fallback owner for domains without trailhead_owner
```

### [server.https] / [server.http] — listeners

```ini
[server.https]
bind = 0.0.0.0:443
tls = true

[server.http]
bind = 0.0.0.0:80
tls = false
```

### [php.*] — PHP processor

```ini
[php.*]
processor = ast                      ; ast | cgi | embedded | auto
binary = /usr/local/bin/php-cgi      ; only needed for cgi mode
```

### [https.&lt;domain&gt;] — virtual hosts

Each domain gets its own section with a docroot. Comma-separate for aliases:

```ini
[https.example.com]
docroot = /var/www/live/ruph_root/example.com
access_log = /var/www/live/ruph_logs/example.log  ; per-domain access log
error_log = /var/www/live/ruph_logs/example_error.log
trailhead_owner = example   ; logs go to Trailhead instead of access_log file

[https.a.com,https.b.com]
docroot = /var/www/live/ruph_root/shared   ; both domains share a docroot
trailhead_owner = shared
```

**Per-domain keys:**

| Key | Description |
|-----|-------------|
| `docroot` | Document root for this domain |
| `access_log` | Access log file (overrides `[server]` access_log) |
| `error_log` | Error log file (overrides `[server]` error_log) |
| `trailhead_owner` | Trailhead log group owner; when set, skips file access_log |

### Prefix matching

Section names without dots match as prefixes. `[https.www]` matches any
`www.*` hostname (used to redirect www to non-www).

### [http.*] — default HTTP docroot

```ini
[http.*]
docroot = /var/www/live/ruph_root/ruph_http  ; serves all plain HTTP requests
```

## Logging Priority

For each request, logging destinations are evaluated independently:

1. **Console** — `tracing` output (if `log_console = true`)
2. **Access log file** — written unless `trailhead_owner` resolves for the domain
3. **Error log file** — always written (PHP errors, AST warnings)
4. **SQLite** (`log_full`) — full request record to `.db` (if configured)
5. **Trailhead API** — full request record as NDJSON (if `trailhead_owner` resolves)

When `trailhead_owner` is set for a domain, the access log file is skipped because
Trailhead captures a superset of the data (all request/response headers, timing, etc.).
