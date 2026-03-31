# ruph — Rust + PHP

A single-binary web server with built-in PHP scripting. No PHP installation required.

ruph includes a native PHP interpreter powered by tree-sitter, giving you PHP's familiar syntax and templating for web sites and apps — all compiled into one cross-platform binary. For advanced use cases, ruph can optionally use an external PHP binary as a fallback.

If you just want to get a site running quickly, start with [QUICKSTART.md](QUICKSTART.md).

## Quick Start

```bash
# Serve a directory — that's it
./ruph /var/www/mysite

# With TLS
./ruph /var/www/mysite --tls

# With a config file
./ruph -c ruph.ini /var/www/mysite
```

Create a `_index.php` in your docroot and you have a PHP-powered site:

```php
<?php
// _index.php — runs for every request
if ($_SERVER['REQUEST_URI'] === '/') {
    readfile('./index.html');
} else {
    http_response_code(404);
    echo "Not found";
}
```

Running `./ruph` with no arguments prints help.

## How It Works

### Request Flow

```
Request arrives
    │
    ├── Select effective vhost docroot
    ├── ruph resolves URI against filesystem
    │   Sets: $_SERVER['rr_file'], rr_dir, rr_index, rr_leaf_idx, rr_mime
    │
    ├── Execute global /_index.php (master), if present
    ├── Execute vhost-root /_index.php, if present
    │
    ├── Existing .php file target → execute it
    ├── Existing static/index target + deepest local leaf _index.php
    │   ├── leaf handles → DONE
    │   └── leaf passes through → Rust delivers target
    │
    ├── Existing static/index target, no leaf → Rust delivers target
    └── Missing path → deepest local leaf handles, or 404
```

### _index.php Architecture

There are up to **three** controller opportunities per request:

1. **Global master** (`/<server-root>/_index.php`)
2. **Vhost-root controller** (`/<vhost-docroot>/_index.php`)
3. **Deepest local leaf** (`/subdir/_index.php`)

Only the deepest local leaf runs. Intermediate `_index.php` files do not form a chain.

The intended model is:
- global master = server-wide policy
- vhost-root `_index.php` = site-wide policy
- deepest local leaf `_index.php` = local interception/routing
- otherwise Rust serves the resolved target directly

See [REQUESTS.md](REQUESTS.md) for the exact request flow and controller semantics.

```php
<?php
// Master _index.php — global auth, then delegate
session_start();
if (!authenticated()) { http_response_code(401); exit; }

// Let the leaf _index.php handle it, or ruph serves static
```

### Pre-resolved Request Info

ruph resolves the filesystem **before** PHP runs and sets these `$_SERVER` keys:

| Key | Value |
|-----|-------|
| `rr_root` | Absolute path to the vhost document root |
| `rr_file` | Realpath of matched file, or empty |
| `rr_exists` | `"1"` if URI maps to an existing file, or empty |
| `rr_dir` | Realpath if URI maps to a directory, or empty |
| `rr_index` | First matching index file inside `rr_dir`, or empty |
| `rr_leaf_idx` | Deepest relevant local `_index.php` below the vhost root, or empty |
| `rr_mime` | MIME type ruph would use for `rr_file` |

Plus all standard `$_SERVER` keys: `REQUEST_URI`, `REQUEST_METHOD`, `QUERY_STRING`, `HTTP_HOST`, `DOCUMENT_ROOT`, `SCRIPT_FILENAME`, `PHP_SELF`, `PATH_INFO`, and all `HTTP_*` headers.

For full details on request flow, controller behavior, and `rr_*` variables, see [REQUESTS.md](REQUESTS.md).

## Ruph PHP Reference

ruph's built-in interpreter runs PHP natively in Rust — no external PHP binary needed. It covers the PHP features used in web templating and request handling.

### Language Features

| Feature | Support |
|---------|---------|
| `<?php ?>` and `<?= ?>` tags | Full |
| Inline HTML mixed with PHP | Full |
| Variables, assignment, string interpolation | Full |
| `if` / `else` / `elseif` | Full |
| `while`, `for`, `foreach` | Full |
| `switch` / `case` / `default` | Full |
| `break`, `continue`, `return` | Full |
| `exit` / `die` | Full |
| `include` / `require` / `_once` variants | Full |
| User-defined functions with defaults | Full |
| Arrays (indexed and associative) | Full |
| `$_GET`, `$_POST`, `$_SERVER`, `$_REQUEST` | Full |
| String concatenation (`.`), `??`, `?:` | Full |
| All comparison: `==`, `===`, `!=`, `!==`, `<`, `>`, `<=`, `>=` | Full |
| Arithmetic: `+`, `-`, `*`, `/`, `%`, `**` | Full |
| Logical: `&&`, `\|\|`, `!`, `and`, `or` | Full |
| Augmented assignment: `+=`, `-=`, `.=`, `??=` | Full |
| `++` / `--` (prefix and postfix) | Full |
| Type casting: `(int)`, `(string)`, `(bool)`, `(float)`, `(array)` | Full |
| Output buffering: `ob_start()`, `ob_get_clean()` | Full |
| `define()` / `defined()` constants | Full |

### Built-in Functions

#### String

`strlen`, `strtolower`, `strtoupper`, `trim`, `ltrim`, `rtrim`, `substr`, `str_replace`, `str_contains`, `str_starts_with`, `str_ends_with`, `strpos`, `stripos`, `strrpos`, `explode`, `implode`/`join`, `sprintf`, `nl2br`, `htmlspecialchars`, `htmlspecialchars_decode`, `htmlentities`, `urlencode`, `urldecode`, `rawurlencode`, `rawurldecode`, `ucfirst`, `lcfirst`, `str_repeat`, `str_pad`, `number_format`, `md5`

#### Array

`count`/`sizeof`, `array_keys`, `array_values`, `in_array`, `array_key_exists`, `array_merge`, `array_push`, `array_pop`, `array_slice`, `array_reverse`, `array_unique`, `array_map`, `range`, `compact`, `extract`, `sort`, `rsort`, `asort`, `arsort`, `ksort`, `krsort`

#### Type

`isset`, `empty`, `is_null`, `is_array`, `is_string`, `is_numeric`, `is_int`/`is_integer`, `is_bool`, `gettype`, `intval`, `floatval`/`doubleval`, `strval`, `boolval`

#### JSON

`json_encode`, `json_decode`

#### Filesystem

`file_exists`, `is_file`, `is_dir`, `readfile`, `file_get_contents`, `file_put_contents`, `filesize`, `dirname`, `basename`, `pathinfo`, `realpath`, `glob`

#### Date/Time

`date`, `time`, `microtime`, `strtotime`

#### Math

`abs`, `ceil`, `floor`, `round`, `max`, `min`, `rand`/`mt_rand`

#### Regex

`preg_match`, `preg_replace`, `preg_split`

#### HTTP/Output

`header`, `http_response_code`, `setcookie`, `echo`, `print_r`, `var_dump`, `error_log`

#### Misc

`phpversion`, `php_uname`, `php_sapi_name`, `function_exists`, `define`, `defined`, `constant`, `sleep`, `usleep`, `session_start`, `session_destroy`

#### Ruph Extensions

| Function | Description |
|----------|-------------|
| `exe($path)` | Execute a path like the web server: `exe('dir/')` runs `dir/_index.php` |
| `render($template, $data)` | Render a PHP template with data |
| `response($status, $headers, $body)` | Set response status, headers, and body in one call |
| `http_request($method, $url, $headers, $body)` | Make an HTTP request from PHP |
| `file_get_contents($url)` | Works with both local files and HTTP URLs |

### Pre-defined Constants

`PATHINFO_DIRNAME` (1), `PATHINFO_BASENAME` (2), `PATHINFO_EXTENSION` (4), `PATHINFO_FILENAME` (8), `PHP_EOL`, `PHP_INT_MAX`, `PHP_INT_MIN`, `DIRECTORY_SEPARATOR`, `PHP_SAPI`, `PHP_VERSION`, `TRUE`, `FALSE`, `NULL`

### What's Not Included

ruph PHP is designed for web scripting, not running legacy PHP applications. The following are **not** supported:

- Classes and objects (`class`, `new`, `->`, `::`)
- Namespaces and `use` statements
- Anonymous functions / closures
- Generators and `yield`
- Try/catch exceptions
- PHP extensions (PDO, curl, mbstring, etc.)
- `$_SESSION` persistence (session functions exist as stubs)

For sites that need full PHP, set `processor = cgi` in your config and ruph will use an external PHP binary as a subprocess.

## PHP Processing Modes

| Mode | How | External PHP? | Use Case |
|------|-----|---------------|----------|
| **`auto`** (default) | Built-in AST interpreter, fallback to CGI | No (optional) | Single-binary deployment |
| **`ast`** | Built-in only, no fallback | No | Guaranteed single-binary |
| **`cgi`** | External php-cgi subprocess | Yes | Full PHP compatibility |

Set via `[php] processor` in `ruph.ini`:

```ini
[php]
; auto (default) | ast | cgi
processor = auto
; Only needed for cgi mode (auto-detected if omitted)
;binary = /usr/local/bin/php-cgi
```

## CLI Options

```
Usage: ruph [OPTIONS] [DOCROOT]

Arguments:
  [DOCROOT]  Root directory to serve

Options:
      --bind-https <ADDR>      HTTPS bind address (default from config)
      --bind-http <ADDR>       Optional plain-HTTP bind address
  -c, --config <FILE>          Configuration file (INI format)
      --new-cert <NEW_CERT>    Generate ACME/Let's Encrypt cert: email@domain,example.com
      --list-certs             List known certificates and exit
      --tls                    Enable TLS (uses certs from ~/.ruph/ssl)
      --php-binary <BINARY>    PHP binary for cgi mode (overrides config)
      --log-level <LOG_LEVEL>  Log level: error, warn, info, debug, trace [default: info]
  -h, --help                   Print help
```

## Configuration (INI)

ruph looks for `ruph.ini` in these locations (first match wins):

1. `<docroot>/ruph.ini`
2. `./ruph.ini`
3. `~/.ruph/ruph.ini`
4. `/etc/ruph/ruph.ini`

### Example ruph.ini

```ini
[server]
bind = 0.0.0.0:8082
log_level = info
tls = false

[http]
docroot = /var/www/html
index_files = _index.php,index.html

[php]
; auto | ast | cgi
processor = auto
```

## TLS / ACME Certificates

Certificates are stored per-domain in `~/.ruph/ssl/<domain>/`.

```bash
# Issue a Let's Encrypt certificate
./ruph --new-cert you@example.com,example.com

# List certificates and expiry dates
./ruph --list-certs
```

Multiple domains are supported via SNI. Certificates expiring within 30 days generate a warning at startup.

## Building

```bash
cargo build --release
```

The single binary is at `target/release/ruph`.

### Cross-platform builds

```bash
# macOS (Apple Silicon)
cargo build --release --target aarch64-apple-darwin

# macOS (Intel)
cargo build --release --target x86_64-apple-darwin

# Windows
cargo build --release --target x86_64-pc-windows-msvc

# Linux (musl, fully static)
cargo build --release --target x86_64-unknown-linux-musl
```

### Running tests

```bash
cargo test
```
