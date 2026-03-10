# Return Semantics and rr_* Server Variables

## Return Values from _index.php

Both master and leaf `_index.php` scripts follow the same contract:

| PHP action | Meaning | What ruph does |
|------------|---------|----------------|
| `exit` / `die` | Hard stop — request handled | Sends PHP output + headers as the HTTP response. Done. |
| `return true` | Pass through — let Rust serve | Ruph serves the static file directly (fast path). |
| `return false` | Handled — even if output is empty | Sends PHP output + headers as the HTTP response. Done. |
| `return` (bare) | Same as `return false` | Sends PHP output + headers. Done. |
| Reaching end of script | Auto-detect | Inferred from output/headers/status (see below). |

### Auto-Detection (no explicit return)

When a script falls off the end without `return` or `exit`, ruph infers whether it handled the request:

**Handled** (send PHP response) if any of:
- Non-empty output (echo, readfile, etc.)
- `Location` header set
- Non-200 status code

**Pass through** (let Rust serve) if:
- No output, no headers changed, status 200

This means scripts "just work" without needing `exit` — output a redirect or echo content and ruph sends it. Stay silent and ruph serves the static file.

### When to Use Each

| Scenario | Recommended |
|----------|-------------|
| File exists, serve it fast | `return true` |
| Redirect | `header("Location: ...", true, 301);` (auto-detected, or add `return false` to be explicit) |
| Custom error page | `http_response_code(404); readfile('404.html');` (auto-detected) |
| Auth gate | `http_response_code(401); exit;` |
| Preprocessing only (logging, etc.) | Just run your code — no return needed |
| Explicitly signal "I did nothing" | `return true` |
| Explicitly signal "I'm done" even with no output | `return false` |

### Processing Cascade

```
Master _index.php runs
  |
  +-- exit              --> response sent (done)
  +-- return false      --> response sent (done)
  +-- output/headers    --> response sent (done)  [auto-detected]
  +-- return true       --> pass through:
  +-- silent (no return)--> pass through:
        |
        +-- Leaf _index.php exists?
        |     |
        |     +-- (same rules as master)
        |     +-- pass through:
        |
        +-- rr_file set       --> Rust serves static file
        +-- rr_dir + rr_index --> Rust serves index file
        +-- rr_dir only       --> 500 (directory needs _index.php)
        +-- nothing matched   --> 404
```

## rr_* Server Variables

Ruph resolves the filesystem **before** PHP runs. All paths are canonicalized (symlinks resolved, `../../` blocked). These are set in `$_SERVER`:

| Variable | Type | Description |
|----------|------|-------------|
| `rr_root` | string | Absolute path to the vhost document root. Same as `DOCUMENT_ROOT`. |
| `rr_file` | string\|"" | Realpath of the matched file, or empty if URI doesn't map to a file. `_index.php` files are never exposed here. |
| `rr_exists` | "1"\|"" | `"1"` if the URI maps to an existing file on disk (same file as `rr_file`), empty otherwise. Use `!empty($_SERVER['rr_exists'])` for a fast existence check — no filesystem call needed from PHP. |
| `rr_dir` | string\|"" | Realpath if the URI maps to a directory, or empty. |
| `rr_index` | string\|"" | First matching index file (e.g. `index.html`) found inside `rr_dir`, or empty. Controlled by `index_files` config. |
| `rr_leaf_idx` | string\|"" | Realpath of a `_index.php` found in the deepest existing directory of the URI path, or empty. Never points to the master `_index.php`. |
| `rr_mime` | string\|"" | MIME type ruph would use if serving `rr_file` statically (e.g. `text/html`, `image/png`). |

### Security

All `rr_*` paths are:
- URL-decoded
- Canonicalized via Rust's `fs::canonicalize()` (resolves symlinks)
- Validated to be within the document root (path traversal blocked)
- Computed before PHP execution (no TOCTOU race with PHP's `file_exists`)

### Common Patterns

**Early return for existing files (recommended):**
```php
<?php
// If the file exists on disk, let Rust serve it directly — fastest path
if (!empty($_SERVER['rr_exists']))
    return true;

// ... redirects, auth, custom routing below ...
```

**Redirect (no exit needed):**
```php
<?php
header("Location: https://example.com{$_SERVER['REQUEST_URI']}", true, 301);
// Auto-detected as handled — no exit or return needed
```

**Check if request targets a directory:**
```php
<?php
if (!empty($_SERVER['rr_dir'])) {
    if (empty($_SERVER['rr_index'])) {
        http_response_code(403);
        echo "Directory listing not allowed";
        // Auto-detected: non-empty output = handled
    }
}
```

**Leaf _index.php serving files with custom headers:**
```php
<?php
$file = $_SERVER['rr_file'];
if (!empty($file)) {
    header('X-Served-By: leaf');
    header('Content-Type: ' . $_SERVER['rr_mime']);
    header('Content-Length: ' . filesize($file));
    readfile($file);
    // Auto-detected: non-empty output = handled
}
// No file matched — silent = pass through to 404
```

**Using rr_root for filesystem operations:**
```php
<?php
$root = $_SERVER['rr_root'];
$custom_404 = $root . '/errors/404.html';
if (is_file($custom_404)) {
    http_response_code(404);
    readfile($custom_404);
    // Auto-detected: output + non-200 status = handled
}
```

## Relationship to Other Docs

- [README.md](README.md) — Overview of ruph, request flow diagram, quick start
- [RUPH-PHP.md](RUPH-PHP.md) — Complete PHP language and function reference
- [RUPH_INI.md](RUPH_INI.md) — Configuration file reference
- [REQUESTS.md](REQUESTS.md) — Detailed request processing internals (host resolution, method behavior, CGI specifics)
