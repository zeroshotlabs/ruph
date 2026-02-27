# ruph

A Rust + PHP web server with built-in PHP processing, TLS/ACME certificate management, and front-controller routing.

## Quick Start

```bash
# Serve current directory
./ruph .

# Serve a specific docroot
./ruph /var/www/html

# With TLS (uses certs from ~/.ruph/ssl)
./ruph /var/www/html --tls

# With a config file
./ruph /var/www/html -c ruph.ini
```

Running `./ruph` with no arguments prints help.

## CLI Options

```
Usage: ruph [OPTIONS] [DOCROOT]

Arguments:
  [DOCROOT]  Root directory to serve

Options:
      --bind <BIND>            Bind address [default: 0.0.0.0:8082]
  -c, --config <FILE>          Configuration file (INI format)
      --new-cert <NEW_CERT>    Generate ACME/Let's Encrypt cert: email@domain.com,example.com
      --list-certs             List known certificates and exit
      --tls                    Enable TLS (uses certs from ~/.ruph/ssl)
      --log-level <LOG_LEVEL>  Log level: error, warn, info, debug, trace [default: info]
  -h, --help                   Print help
```

## Configuration (INI)

ruph looks for `ruph.ini` in these locations (first match wins):

1. `<docroot>/ruph.ini`
2. `./ruph.ini` (current directory)
3. `~/.ruph/ruph.ini`
4. `/etc/ruph/ruph.ini`

Or specify explicitly with `-c /path/to/ruph.ini`.

CLI arguments always override config file values.

### Example ruph.ini

```ini
[server]
bind = 0.0.0.0:8082
log_level = info
tls = false

[http]
docroot = /var/www/html
index_files = _index.php

[php]
; auto | libphp | ast | embedded
processor = auto
; Path to PHP binary (auto-detected if omitted)
;binary = /usr/local/bin/php

[ssl]
; Override certificate directory (default: ~/.ruph/ssl)
;dir = /etc/ruph/ssl
```

## PHP Processing

ruph has three PHP processors that execute in a configurable chain:

| Processor | Description |
|-----------|-------------|
| **ast** | Tree-sitter PHP parser. Handles templates, includes, superglobals, output buffering. |
| **embedded** | Lightweight regex-based processor. Handles echo, variables, basic functions. |
| **libphp** | External PHP binary (php-cli). Full PHP support via subprocess. |

### Processor Modes

Set via `[php] processor` in `ruph.ini`:

| Mode | Execution Order | Use Case |
|------|----------------|----------|
| `auto` (default) | ast -> embedded -> libphp | Fastest for simple templates, falls back for complex PHP |
| `libphp` | libphp -> ast -> embedded | Full PHP compatibility first |
| `ast` | ast only | Tree-sitter parsing only |
| `embedded` | embedded only | Regex processing only |

Each processor falls through to the next on failure or empty output.

Example - force external PHP as primary:

```ini
[php]
processor = libphp
binary = /usr/local/bin/php
```

## Front Controller (_index.php)

If a `_index.php` file exists in the docroot, it acts as a front controller for all unmatched requests (similar to nginx `try_files` or Apache `FallbackResource`).

The routing order is:

1. Exact file match -> serve static file or execute PHP script
2. Directory match -> look for `_index.php` in that directory
3. No match -> route to `<docroot>/_index.php` (if it exists)
4. No `_index.php` -> return 404

POST requests to non-script targets also route through the front controller.

Available to `_index.php` via superglobals:

- `$_SERVER['REQUEST_URI']` - the original requested path
- `$_SERVER['REQUEST_METHOD']` - GET, POST, etc.
- `$_SERVER['QUERY_STRING']` - original query string
- `$_SERVER['SCRIPT_FILENAME']` - path to `_index.php` itself
- `$_SERVER['DOCUMENT_ROOT']` - the docroot
- `$_SERVER['PHP_SELF']` - script name relative to docroot
- `$_SERVER['PATH_INFO']` - path info after the script name
- `$_SERVER['HTTP_HOST']` - requested hostname
- All HTTP headers as `$_SERVER['HTTP_*']`

## TLS / ACME Certificates

Certificates are stored per-domain in `~/.ruph/ssl/<domain>/`:

```
~/.ruph/ssl/
  example.com/
    fullchain.pem
    privkey.pem
  other.com/
    fullchain.pem
    privkey.pem
```

### Issue a certificate via Let's Encrypt

```bash
./ruph --new-cert you@example.com,example.com
```

### List certificates and expiry dates

```bash
./ruph --list-certs
```

Certificates expiring within 30 days generate a warning at startup. Multiple domains are supported via SNI - ruph selects the correct certificate based on the requested hostname.

## Logging

ruph uses structured logging with domain names in every log line:

```
INFO  ruph: TLS established from 1.2.3.4:5678 [example.com]
INFO  ruph: 1.2.3.4:5678 GET /page.php [example.com]
ERROR ruph: TLS handshake error from 5.6.7.8:9999 [bad.com]: no server certificate chain resolved
ERROR ruph: TLS accept error from 9.8.7.6:1234 (no ClientHello): received corrupt message
```

Set log level via CLI (`--log-level debug`) or config (`log_level = debug`).

## Building

```bash
cargo build --release
```

The binary is at `target/release/ruph`.

### Running tests

```bash
cargo test
```
