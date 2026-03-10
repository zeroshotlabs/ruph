# Request Processing Behavior (Current Implementation)

This document describes how `ruph` currently resolves requests and decides when PHP executes.

For `rr_*` server variables and return semantics, see [RETURN_RR_VARS.md](RETURN_RR_VARS.md).
For configuration, see [RUPH_INI.md](RUPH_INI.md).

## TLDR;

Processing cascade for the _index.php controller files.

- Master exit / return false / output → response (done)
- Master return true / silent         → Leaf exit / return false / output → response (done)
- Master return true / silent         → Leaf return true / silent → Rust serves static file (fast)
- Master return true / silent         → No leaf → Rust serves static file (fast)

## 1) Host Resolution and Docroot Selection

Routing host is determined in this order:

1. For TLS requests, SNI is copied into `Host` and used as authoritative routing host.
2. Otherwise, `Host` header is used if present.
3. Otherwise, URI authority (`:authority`, relevant for HTTP/2) is used if present.

Then effective docroot is chosen in this order:

1. Exact host match in `domain_roots` (`example.com`).
2. Longest matching host prefix in `prefix_roots` (`www*` style logic).
3. Fallback to default `root_dir`.

Host matching for exact and prefix virtual hosts is case-insensitive.

Code: `main::handle_request`, `WebServer::handle_request`, `WebServer::effective_root`.

## 2) `_index.php` Master/Leaf Architecture

After docroot selection, ruph looks for a master `_index.php` at the docroot root. The middleware index filename is chosen as:

1. first configured `index_files` entry ending in `.php`,
2. or `_index.php` if none are configured.

At most **two** PHP scripts execute per request:

1. **Master** (`/<docroot>/_index.php`) — always runs first. Server admin controls this.
2. **Leaf** (`/<subdir>/_index.php`) — runs only if master passes through and a leaf exists in the deepest matching directory.

### Handled vs Pass-Through

A script is considered to have **handled** the request if any of:

| Signal | Meaning |
|--------|---------|
| `exit` / `die` | Hard stop — always handled |
| `return false` / `return` (bare) | Explicit handled |
| Non-empty output (echo, readfile) | Auto-detected as handled |
| `Location` header set | Auto-detected as handled |
| Non-200 status code | Auto-detected as handled |

A script **passes through** if:

| Signal | Meaning |
|--------|---------|
| `return true` | Explicit pass-through |
| Silent (no output, no headers, status 200, no return) | Auto-detected as pass-through |

Code: `php_handled_request`, `handle_request`.

## 3) Pre-Resolved Filesystem (`rr_*` Variables)

Before PHP runs, ruph resolves the request URI against the filesystem with path-traversal protection and sets `$_SERVER` keys:

- `rr_root` — vhost document root
- `rr_file` — realpath of matched file (never `_index.php`)
- `rr_exists` — `"1"` if file exists, empty otherwise
- `rr_dir` — realpath if URI maps to a directory
- `rr_index` — first matching index file inside `rr_dir`
- `rr_leaf_idx` — `_index.php` in deepest matching directory
- `rr_mime` — MIME type for `rr_file`

See [RETURN_RR_VARS.md](RETURN_RR_VARS.md) for full details.

Code: `resolve_rr_vars`.

## 4) Standard `$_SERVER` Variables

All standard CGI/PHP server variables are populated:

| Variable | Source |
|----------|--------|
| `REQUEST_URI` | Normalized path+query (e.g. `/page.html?x=1`) |
| `REQUEST_METHOD` | `GET`, `POST`, `HEAD` |
| `QUERY_STRING` | Raw query string |
| `HTTP_HOST` | From `Host` header |
| `DOCUMENT_ROOT` | Vhost document root (same as `rr_root`) |
| `SCRIPT_FILENAME` | Absolute path to the executing PHP script |
| `PHP_SELF` | URI path of the script |
| `PATH_INFO` | Extra path segments after script name |
| `SERVER_NAME` | Hostname |
| `SERVER_PORT` | Listen port |
| `HTTPS` | `"on"` for TLS connections |
| `HTTP_*` | All request headers as `HTTP_` prefixed vars |

All HTTP request headers are automatically mapped: `Referer` → `HTTP_REFERER`, `User-Agent` → `HTTP_USER_AGENT`, `Accept` → `HTTP_ACCEPT`, etc.

Code: `build_server_vars`.

## 5) Request Target Resolution

For request path `P`, resolution is:

1. If `P` exists and is a file:
   - `.php` extension => `Script`
   - otherwise => `Static`
2. Else if `P` exists and is a directory:
   - scan that directory for first matching `index_files` entry (in configured order)
   - `.php` index => `Script`
   - non-php index => `Static`
3. Else:
   - if root init/front-controller script exists and is a file => `Script` (fallback)
   - else => `NotFound`

Code: `resolve_request_target`, `find_index_file`.

## 6) Method Behavior

### GET

- `Static` => file is served.
- `Script` => PHP pipeline executes the script.
- `NotFound` => 404.

Code: `handle_get_request`.

### POST

- If resolved target is `Script`, that script executes.
- If resolved target is `Static` or `NotFound`:
  - if root init/front-controller script exists and is a file, request is routed to it;
  - otherwise 404.

So POST to an existing static file can still execute root front controller.

Code: `handle_post_request`.

### HEAD

- `Static` => metadata response (no body).
- `Script` => 405 (HEAD not supported for scripts).
- `NotFound` => 404.

Code: `handle_head_request`.

## 7) PHP Execution Order for Script Targets

When a target is `Script`, `process_php_template` applies:

1. If script target is not a file => 404 (`Script not found`).
2. If no processors available at all => serve PHP file as static bytes.
3. In `cgi` or `auto` mode, try external PHP streaming first.
4. If streaming fails, fall back to configured chain (`ast`/`embedded`/`cgi` fallback path).

Important:

- `php_mode` affects script execution and middleware CGI behavior.

Code: `process_php_template`.

## 8) Hierarchy Clarification for `_index.php` + Static Files

Given:

- `/static/_index.php` exists
- `/static/img.gif` exists
- request is `GET /static/img.gif`

Current behavior:

1. Master `/_index.php` runs.
2. If master passes through, leaf `/static/_index.php` runs.
3. If leaf handles (output, headers, return false, or exit) — static file is **not** served.
4. If leaf passes through (return true, or silent) — Rust serves `/static/img.gif`.

If `/static/img.gif` does not exist and no script handled:

- 404 is returned.

If `/static/` is requested (directory path):

- The server checks `index_files` inside `/static/` in configured order.
- That means `index.html`, `_index.php`, etc. precedence is controlled only by `index_files` order.

## 9) Root Front-Controller Fallback

If no file/directory index target matches, request falls back to root front-controller script (same selected middleware PHP index filename at docroot root) when it exists as a file.

## 10) Static File Delivery

When Rust serves a static file (after PHP passes through), it delivers with:

- `Content-Type` — detected from file extension via `mime_guess`
- `Content-Length` — exact byte count

This is the fastest path — no PHP overhead, direct file read and response.
