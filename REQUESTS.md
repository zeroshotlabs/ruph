# Request Processing Behavior

This document is the source of truth for how `ruph` resolves requests and decides when PHP executes.

For configuration, see [RUPH_INI.md](RUPH_INI.md). For the built-in PHP language reference, see [RUPH-PHP.md](RUPH-PHP.md).

## TLDR

Request flow:

1. Resolve the effective vhost docroot from the request host.
2. Populate all `rr_*` values before any PHP runs.
3. Intercept the status page path, if configured (before any PHP).
4. Run the global master `/_index.php` from the server root, if present.
5. Run the effective vhost root `/_index.php`, if present and different from the global master.
6. Resolve the request target.
7. If the target is an existing `.php` file, execute that file directly.
8. Otherwise, if a deepest local leaf `_index.php` exists below the vhost root, execute that single leaf.
9. If the leaf passes through, deliver the resolved file/index directly.
10. If nothing resolves, return 404.

Only one local leaf `_index.php` ever runs.

## 1) Host Resolution and Docroot Selection

Routing host is determined in this order:

1. For TLS requests, SNI is copied into `Host` and used as authoritative routing host.
2. Otherwise, `Host` header is used if present.
3. Otherwise, URI authority (`:authority`) is used if present.

Then effective docroot is chosen in this order:

1. Exact host match in `domain_roots`
2. Longest matching host prefix in `prefix_roots`
3. Fallback to default `root_dir`

Host matching for virtual hosts is case-insensitive.

## 2) `rr_*` Variables

Before any PHP executes, ruph resolves the request URI against the effective vhost docroot and populates these `$_SERVER` keys:

| Key | Value |
|-----|-------|
| `rr_root` | Effective vhost document root |
| `rr_file` | Realpath of the literal matched file, or empty |
| `rr_exists` | `"1"` if the literal request path maps to an existing file, else empty |
| `rr_dir` | Realpath if the literal request path maps to a directory, else empty |
| `rr_index` | First configured content index file found inside `rr_dir`, else empty |
| `rr_leaf_idx` | Deepest local `_index.php` below the vhost root relevant to this path, else empty |
| `rr_mime` | MIME type ruph would use for `rr_file` |

Notes:

- `_index.php` is infrastructure, not content. It is excluded from `rr_file` and `rr_index`.
- `rr_index` finds the first non-`_index.php` entry from `index_files` (e.g. `index.html`, `index.php`).
- `rr_leaf_idx` never points at the global master or the vhost-root controller.
- For missing paths, `rr_leaf_idx` is based on the deepest existing directory on disk.

### RUPH_* Server Variables

In addition to the standard CGI variables (`REQUEST_URI`, `REQUEST_METHOD`, `HTTP_HOST`, `DOCUMENT_ROOT`, `SCRIPT_FILENAME`, `QUERY_STRING`, etc.) and `rr_*` variables, ruph injects live server metrics into `$_SERVER`:

| Variable | Description |
|----------|-------------|
| `REMOTE_IP` | Client's IP address |
| `RUPH_IP_HITS` | Total requests from this IP since server start |
| `RUPH_IP_HITS_WINDOW` | Requests from this IP in the last N seconds |
| `RUPH_RATE_WINDOW` | The rate window size in seconds (from config) |
| `RUPH_QPS_10` | Server-wide requests/sec (10-second average) |
| `RUPH_QPS_60` | Server-wide requests/sec (60-second average) |
| `RUPH_TOTAL_REQUESTS` | Total requests since server start |
| `RUPH_ACTIVE_CONNECTIONS` | Current open TCP connections |
| `RUPH_UPTIME` | Server uptime in seconds |

These are available in all three controller layers and in directly-executed PHP files. They enable PHP-side rate limiting without external dependencies — for example, checking `RUPH_IP_HITS_WINDOW > 30` to return 429.

## 3) Status Page Intercept

If `status_page` is configured (e.g. `/status`), requests matching that exact path are handled before any PHP runs. The status page returns an HTML dashboard with live server metrics (active connections, QPS, top IPs, uptime). This intercept cannot be overridden by `_index.php`.

## 4) Controller Layers

There are up to three controller opportunities per request:

1. Global master `/_index.php`
2. Vhost-root `/_index.php`
3. Deepest local leaf `_index.php`

### Global master

The global master lives at the server root used to create the `WebServer`.

It always runs first when present.

Stopping behavior:

- `exit` / `die` => handled, stop
- any explicit `return` value => handled, stop
- output, `Location` header, or non-200 status => handled, stop
- silent fallthrough => continue

### Vhost-root controller

The vhost-root controller is `/_index.php` inside the effective vhost docroot.

It runs after the global master if present and if it is not the same file.

It uses the same stopping behavior as the global master.

### Deepest local leaf

This is the deepest `_index.php` below the vhost root in the relevant directory tree.

Examples:

- `/assets/app.css` => leaf search uses the containing directory of `app.css`
- `/users/` => leaf search uses `/users`
- `/missing/path` => leaf search uses the deepest existing directory on disk

Only this single deepest leaf runs. Intermediate `_index.php` files do not form a chain.

Leaf behavior:

- `return true` => pass through to normal delivery
- `return false` or bare `return` => handled, stop
- `exit` / `die` => handled, stop
- output, `Location` header, or non-200 status => handled, stop
- silent fallthrough => pass through

For missing paths:

- if a deepest leaf exists, it is expected to finalize the response
- if it passes through anyway, ruph returns 500 because there is no file/index to deliver

## 5) Target Resolution

After the controller layers above:

### Existing file

If the literal request path exists and is a file:

- `.php` => execute that file directly as PHP
- anything else => treat as a static file target

Explicit existing `.php` files do not get wrapped by the local leaf `_index.php`.

### Existing directory

If the literal request path exists and is a directory:

- scan `index_files` in configured order, **skipping `_index.php`**
- first matching `.php` index (e.g. `index.php`) => PHP target
- first matching non-PHP index (e.g. `index.html`) => static target
- no matching content index => no direct target

`_index.php` is never selected as a directory index because it has already been handled as a controller. This means a directory with both `_index.php` and `index.html` will serve `index.html` automatically when the controller passes through.

The deepest local leaf may intercept directory requests before the resolved index is delivered.

### Missing path

If the literal request path does not exist:

- no file or directory target exists
- the deepest local leaf gets one chance to handle it
- if there is no such leaf, return 404

## 6) Delivery Rules

If processing reaches final delivery:

- static file target => Rust serves the file directly
- PHP target => execute the PHP file
- no target => 404

This preserves the fast path:

- existing static files are served directly unless a relevant leaf `_index.php` chooses to intercept
- existing `.php` files execute as PHP

## 7) HEAD / GET / POST

### GET

Follows the full controller + target-resolution flow above.

### POST

Follows the same flow as GET, except parsed form data is available to PHP.

### HEAD

- static targets return headers only
- PHP targets return `405 Method Not Allowed`

## 8) Design Goal

The intended mental model is:

- global master = server-wide policy
- vhost-root `_index.php` = site-wide policy for one vhost
- deepest local leaf `_index.php` = local interception/routing
- otherwise ruph serves the resolved file directly

That keeps behavior predictable for novice users while preserving the direct static-file fast path.
