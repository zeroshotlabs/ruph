# ruph.ini Configuration

Config file location: `/var/www/live/ruph_root/ruph.ini`

## Sections

### [server] — global settings

```ini
logs = /path/to/default.log    ; default log file for all domains
index_files = _index.php,index.php,index.html
status_page = /status          ; URL path for status dashboard (omit to disable)
rate_window = 2                ; IP rate-limit sliding window in seconds
log_level = info
log_console = false
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

### [https.<domain>] — virtual hosts

Each domain gets its own section with a docroot. Comma-separate for aliases:

```ini
[https.example.com]
docroot = /var/www/live/ruph_root/example.com
logs = /var/www/live/ruph_logs/example.log   ; optional per-domain log

[https.a.com,https.b.com]
docroot = /var/www/live/ruph_root/shared     ; both domains share a docroot
```

### Prefix matching

Section names without dots match as prefixes. `[https.www]` matches any
`www.*` hostname (used to redirect www to non-www).

### [http.*] — default HTTP docroot

```ini
[http.*]
docroot = /var/www/live/ruph_root/ruph_http  ; serves all plain HTTP requests
```
