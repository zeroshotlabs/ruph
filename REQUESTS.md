# Request Processing Behavior (Current Implementation)

This document describes how `ruph` currently resolves requests and decides when PHP executes.

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

## 2) `_index.php` Middleware Discovery (Top-Down)

After docroot selection, the middleware index filename is chosen as:

1. first configured `index_files` entry ending in `.php`,
2. or `_index.php` if none are configured.

For every request URL, that filename is executed top-down across directories:

- root directory first,
- then each directory component in request path order.

Example:

- `/pipermail/talk/2012-November/031492.html`
- executes (if present): `/_index.php`, `/pipermail/_index.php`, `/pipermail/talk/_index.php`, `/pipermail/talk/2012-November/_index.php`

Execution uses CGI path when available (`php_mode` `cgi`/`auto`) so redirects/header/exit behavior is preserved. If CGI is unavailable/fails, AST init fallback is used.

Code: `middleware_index_name`, `directory_chain_for_path`, `run_directory_index_chain`.

## 3) Middleware Short-Circuit Rules

During top-down middleware execution, request handling stops immediately when a middleware script returns a response that is considered handled:

- non-200 status, or
- `Location` header present, or
- **non-empty response body** (middleware produced output).

If none short-circuit, normal static/script resolution continues.

A `_index.php` that wants to pass through to the next directory or to normal file resolution must produce **no output** (empty body, status 200, no Location header).

Code: `should_short_circuit_middleware`, `run_directory_index_chain`.

## 4) Request Target Resolution

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

## 5) Method Behavior

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

## 6) PHP Execution Order for Script Targets

When a target is `Script`, `process_php_template` applies:

1. If script target is not a file => 404 (`Script not found`).
2. If no processors available at all => serve PHP file as static bytes.
3. In `cgi` or `auto` mode, try external PHP streaming first.
4. If streaming fails, fall back to configured chain (`ast`/`embedded`/`cgi` fallback path).

Important:

- `php_mode` affects this script-execution phase.
- `php_mode` affects script execution and middleware CGI behavior.

Code: `process_php_template`.

## 7) Hierarchy Clarification for `_index.php` + Static Files

Given:

- `/static/_index.php` exists
- `/static/img.gif` exists
- request is `GET /static/img.gif`

Current behavior:

1. Middleware phase executes top-down:
   - `/<docroot>/_index.php`
   - `/static/_index.php`
2. If `/static/_index.php` produces **any output**, it short-circuits — the static file is **not** served.
3. If `/static/_index.php` produces **no output** (empty body, 200, no Location), target resolution proceeds and serves `/static/img.gif`.

This means a `_index.php` that wants to intercept all requests under its directory (including requests that map to real files) just needs to produce output. To pass through transparently, it must produce no output.

If `/static/img.gif` does not exist and no middleware short-circuited:

- Fallback is root init script (`/<docroot>/_index.php`), not nearest-ancestor `/static/_index.php`.

If `/static/` is requested (directory path):

- The server checks `index_files` inside `/static/` in configured order.
- That means `index.html`, `_index.php`, etc. precedence is controlled only by `index_files` order.

## 8) Root Front-Controller Fallback

If no file/directory index target matches, request falls back to root front-controller script (same selected middleware PHP index filename at docroot root) when it exists as a file.

## 9) CGI/PHP Server Vars Note

`REQUEST_URI` passed to PHP is normalized to path+query (for example, `/pipermail/talk/index.html?x=1`), not absolute-form URLs like `https://nyphp.org/...`.
