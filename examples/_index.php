<?php
// =============================================================================
// ruph _index.php — Master/Leaf Behavior Reference
//
// This file demonstrates all the ways an _index.php can interact with ruph.
// Place at docroot for master behavior, or in a subdirectory for leaf behavior.
//
// Flow: Master runs first → if it passes through → Leaf runs → if it passes
//       through → Rust serves static file (fastest path).
//
// Key rr_* server variables (pre-resolved by Rust before PHP runs):
//   rr_root     — document root (same as DOCUMENT_ROOT)
//   rr_file     — realpath of matched file (never _index.php itself)
//   rr_exists   — "1" if file exists on disk, "" otherwise
//   rr_dir      — realpath if URI is a directory
//   rr_index    — first index file found in rr_dir (e.g. index.html)
//   rr_leaf_idx — leaf _index.php in deepest matching directory
//   rr_mime     — MIME type Rust would use for rr_file
// =============================================================================


// -----------------------------------------------------------------------------
// 1. PASS THROUGH — Let Rust serve static files (fastest path)
//
//    If the requested file exists on disk, return true immediately.
//    Rust serves it directly with correct Content-Type — no PHP overhead.
// -----------------------------------------------------------------------------

if (!empty($_SERVER['rr_exists']))
    return true;


// -----------------------------------------------------------------------------
// 2. REDIRECT — Set Location header (auto-detected as handled)
//
//    No exit or return needed — ruph sees the Location header and sends it.
//    But you can add `return false` to be explicit.
// -----------------------------------------------------------------------------

// if ($_SERVER['HTTP_HOST'] === 'old.example.com') {
//     header("Location: https://new.example.com{$_SERVER['REQUEST_URI']}", true, 301);
//     return false;  // explicit, but optional — auto-detected
// }


// -----------------------------------------------------------------------------
// 3. AUTH GATE — Block with status code (auto-detected as handled)
//
//    Any non-200 status is auto-detected as handled.
// -----------------------------------------------------------------------------

// if (!verify_token()) {
//     http_response_code(403);
//     echo 'Forbidden';
//     // auto-detected: non-empty output + non-200 = handled
// }


// -----------------------------------------------------------------------------
// 4. CUSTOM ERROR PAGE — Serve file with error status
// -----------------------------------------------------------------------------

// $custom_404 = $_SERVER['rr_root'] . '/errors/404.html';
// if (!$_SERVER['rr_exists'] && is_file($custom_404)) {
//     http_response_code(404);
//     readfile($custom_404);
//     return false;
// }


// -----------------------------------------------------------------------------
// 5. HARD STOP — exit/die always means "handled"
//
//    Use when you absolutely want to stop all processing.
//    No leaf runs after this, no after-processing possible.
// -----------------------------------------------------------------------------

// if ($blocked) {
//     http_response_code(403);
//     exit;
// }


// -----------------------------------------------------------------------------
// 6. SILENT PASS-THROUGH — Logging/preprocessing without output
//
//    If a script produces no output, sets no headers, and doesn't change the
//    status code, ruph auto-detects it as pass-through. No return needed.
//    Useful for analytics, logging, request timing, etc.
// -----------------------------------------------------------------------------

// error_log("Request: {$_SERVER['REQUEST_METHOD']} {$_SERVER['REQUEST_URI']}");
// — script falls off end, silent = pass-through to leaf or static file


// -----------------------------------------------------------------------------
// 7. ECHO CONTENT — Non-empty output is auto-detected as handled
//
//    Any echo/print/readfile output signals that PHP handled the request.
// -----------------------------------------------------------------------------

// echo '<h1>Hello from PHP</h1>';
// — auto-detected: non-empty output = handled


// -----------------------------------------------------------------------------
// 8. RETURN VALUES — Explicit control over handled vs pass-through
//
//    return true   → pass-through (let Rust serve, or let leaf run)
//    return false  → handled (send PHP response, even if body is empty)
//    return (bare) → same as return false
//    exit / die    → hard stop, always handled
//    (no return)   → auto-detect from output/headers/status
// -----------------------------------------------------------------------------


// -----------------------------------------------------------------------------
// 9. LEAF EXAMPLE — Serve files with custom headers
//
//    A leaf _index.php in a subdirectory (e.g. /static/_index.php) can add
//    headers before letting Rust serve, or serve the file itself.
// -----------------------------------------------------------------------------

// // Option A: Add headers then let Rust serve
// if (!empty($_SERVER['rr_exists'])) {
//     header('X-Served-By: leaf');
//     header('Cache-Control: public, max-age=86400');
//     return true;  // Rust serves the file with these headers added
// }

// // Option B: Serve the file from PHP (for custom logic)
// if (!empty($_SERVER['rr_file'])) {
//     header('Content-Type: ' . $_SERVER['rr_mime']);
//     header('Content-Length: ' . filesize($_SERVER['rr_file']));
//     readfile($_SERVER['rr_file']);
//     // auto-detected: non-empty output = handled
// }


// -----------------------------------------------------------------------------
// 10. ERROR LOGGING — error_log() and trigger_error()
//
//     Both route to the domain's log file (or global fallback).
//     No errors are ever displayed to the client.
// -----------------------------------------------------------------------------

// error_log("Custom message to domain log");
// trigger_error("Warning message", E_USER_WARNING);
// trigger_error("Debug notice", E_USER_NOTICE);


// -----------------------------------------------------------------------------
// 11. DIRECTORY REQUESTS — Check rr_dir and rr_index
// -----------------------------------------------------------------------------

// if (!empty($_SERVER['rr_dir'])) {
//     if (empty($_SERVER['rr_index'])) {
//         http_response_code(403);
//         echo 'Directory listing not allowed';
//     }
//     // If rr_index is set, pass through — Rust serves the index file
//     return true;
// }


// -----------------------------------------------------------------------------
// Default: nothing matched, 404
// -----------------------------------------------------------------------------
http_response_code(404);
echo '404 Not Found';
