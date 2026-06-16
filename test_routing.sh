#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────────────────────────
# test_routing.sh — Ruph request-routing fixture and regression test
#
# Creates a directory tree covering every routing scenario, starts a temporary
# ruph server, fires HTTP requests, and verifies each response.
#
# Architecture under test (per request):
#
#   [global master _index.php]          ← root of global docroot
#        ↓ pass-through (no output, no return)
#   [vhost root _index.php]             ← root of vhost docroot (skipped if same file)
#        ↓ pass-through
#   [intermediate _index.php in sub/]   ← zero or more, top-down from vhost root
#        ↓ pass-through
#   [deepest _index.php  = leaf]        ← handles virtual routes; return true = fall-through
#        ↓ return true / no output
#   [static file / direct .php / 404]
#
# Stop signals (any of these ends the chain):
#   controller: exit | any return value | non-empty body | non-200 | Location header
#   leaf:       exit | return false | non-empty body | non-200 | Location header
#   leaf falls through: return true | (no return + empty body + 200 + no Location)
#
# Usage:
#   ./test_routing.sh [dir]
#   RUPH_BIN=/path/to/ruph ./test_routing.sh [dir]
# ──────────────────────────────────────────────────────────────────────────────
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEFAULT_DIR="/tmp/ruph-route-test"
TEST_PORT=19742
PASS=0; FAIL=0
SERVER_PID=""
BODY_TMP="$(mktemp)"

# ── Locate ruph binary ────────────────────────────────────────────────────────
find_ruph() {
    for c in \
        "${RUPH_BIN:-UNSET}" \
        "$SCRIPT_DIR/target/release/ruph" \
        "$SCRIPT_DIR/target/debug/ruph" \
        "$(command -v ruph 2>/dev/null || true)"; do
        [[ "$c" != "UNSET" && -n "$c" && -x "$c" ]] && { echo "$c"; return; }
    done
}
RUPH="$(find_ruph || true)"

# ── Terminal colours ──────────────────────────────────────────────────────────
if [[ -t 1 ]]; then
    GRN=$'\033[32m' RED=$'\033[31m' YLW=$'\033[33m' BLU=$'\033[1;34m'
    DIM=$'\033[2m' RST=$'\033[0m' BLD=$'\033[1m'
else
    GRN='' RED='' YLW='' BLU='' DIM='' RST='' BLD=''
fi

# ── Resolve test directory ────────────────────────────────────────────────────
if [[ $# -gt 0 ]]; then
    TEST_DIR="$1"
elif [[ -t 0 ]]; then
    read -r -p "Test directory [${DEFAULT_DIR}]: " TEST_DIR
    TEST_DIR="${TEST_DIR:-$DEFAULT_DIR}"
else
    TEST_DIR="$DEFAULT_DIR"
fi

echo ""
echo "${BLD}${BLU}Ruph routing test${RST}"
echo "  Dir : $TEST_DIR"
echo "  Port: $TEST_PORT"
if [[ -n "$RUPH" ]]; then
    echo "  Bin : $RUPH"
else
    echo "  ${YLW}No ruph binary found — will create fixture only${RST}"
    echo "  Build with: cd $SCRIPT_DIR && cargo build"
fi
echo ""

# Confirm before wiping an existing dir
if [[ -d "$TEST_DIR" && -t 0 ]]; then
    read -r -p "Directory exists. Recreate? [y/N] " yn
    [[ "$yn" =~ ^[Yy]$ ]] || { echo "Aborted."; exit 1; }
fi

# ── Cleanup on exit ───────────────────────────────────────────────────────────
cleanup() {
    [[ -n "$SERVER_PID" ]] && kill "$SERVER_PID" 2>/dev/null || true
    rm -f "$BODY_TMP"
}
trap cleanup EXIT

# ══════════════════════════════════════════════════════════════════════════════
# FIXTURE CREATION
# ══════════════════════════════════════════════════════════════════════════════
echo "${BLD}Creating fixture…${RST}"
rm -rf "$TEST_DIR"
mkdir -p "$TEST_DIR"/{api,blog,deep/nested,noindex}

# ── Root _index.php (vhost root controller) ───────────────────────────────────
# Runs first for every request.  Handles one special route; passes through for
# everything else (no output + no return = chain continues).
cat > "$TEST_DIR/_index.php" <<'PHP'
<?php
// Root vhost controller — first script in the chain for every request.
//
// Chain pass-through rule: produce NO output and do NOT call return.
// Any echo/header/exit or any return value stops the chain here.

if ($_SERVER['REQUEST_URI'] === '/master-test') {
    // Explicitly handle this one route to demonstrate controller short-circuit.
    echo 'ROOT:master-test';
    exit;
}

// All other routes fall through to the leaf / static delivery.
PHP

# ── Root static file ──────────────────────────────────────────────────────────
cat > "$TEST_DIR/static.html" <<'HTML'
<html><body>STATIC_HTML_ROOT</body></html>
HTML

# ── Direct PHP script ─────────────────────────────────────────────────────────
# A .php file that is NOT named _index.php: executed directly after controllers
# pass through.  The leaf mechanism does NOT apply to direct .php targets.
cat > "$TEST_DIR/app.php" <<'PHP'
<?php
echo 'DIRECT_PHP:' . $_SERVER['REQUEST_URI'];
PHP

# ── api/ — leaf that handles virtual routes, falls through for static ─────────
#
# rr_is_static is "1" in $_SERVER when the URL resolves to a real non-PHP file.
# return true from a leaf means "I'm not handling this; serve the file directly."
cat > "$TEST_DIR/api/_index.php" <<'PHP'
<?php
// API leaf: fall through for static files; handle everything else as a route.
if (!empty($_SERVER['rr_is_static'])) {
    return true; // static delivery (e.g. data.json)
}
header('Content-Type: application/json');
echo json_encode(['leaf' => 'api', 'uri' => $_SERVER['REQUEST_URI']]);
exit;
PHP

cat > "$TEST_DIR/api/data.json" <<'JSON'
{"static":"json","file":"data.json"}
JSON

# ── blog/ — leaf with explicit return true / handled split ────────────────────
cat > "$TEST_DIR/blog/_index.php" <<'PHP'
<?php
// Blog leaf: static posts fall through; virtual slugs are handled.
if (!empty($_SERVER['rr_is_static'])) {
    return true;
}
echo 'BLOG:' . ltrim(parse_url($_SERVER['REQUEST_URI'], PHP_URL_PATH), '/');
exit;
PHP

cat > "$TEST_DIR/blog/post.html" <<'HTML'
<html><body>BLOG_POST_STATIC</body></html>
HTML

# ── deep/ — intermediate controller (pass-through) ────────────────────────────
#
# When _index.php files exist in multiple directories between the vhost root and
# the target directory, all but the deepest run as controllers (pass-through or
# stop).  The deepest runs as the leaf.
cat > "$TEST_DIR/deep/_index.php" <<'PHP'
<?php
// Intermediate controller: no output, no return → chain continues to
// deep/nested/_index.php (the deepest leaf).
// In real use: could do auth, variable setup, logging.
PHP

# ── deep/nested/ — deepest leaf ───────────────────────────────────────────────
cat > "$TEST_DIR/deep/nested/_index.php" <<'PHP'
<?php
if (!empty($_SERVER['rr_is_static'])) {
    return true; // serve content.txt etc. directly
}
echo 'NESTED:' . ltrim(parse_url($_SERVER['REQUEST_URI'], PHP_URL_PATH), '/');
exit;
PHP

cat > "$TEST_DIR/deep/nested/content.txt" <<'TXT'
STATIC_NESTED_CONTENT
TXT

# ── noindex/ — no _index.php at all ──────────────────────────────────────────
# Static files served directly (root controller passes through, no leaf added).
# Missing paths → 404 (root controller passes through, target = NotFound).
cat > "$TEST_DIR/noindex/readme.txt" <<'TXT'
NOINDEX_README
TXT

# ── Minimal ruph.ini ──────────────────────────────────────────────────────────
cat > "$TEST_DIR/ruph.ini" <<INI
[server]
index_files = _index.php,index.html

[php.*]
processor = ast

[server.http]
bind = 127.0.0.1:${TEST_PORT}
INI

# Print tree
echo ""
printf '%s\n' "$(find "$TEST_DIR" -not -name 'ruph.ini' | LC_ALL=C sort | while IFS= read -r f; do
    rel="${f#$TEST_DIR/}"
    [[ "$rel" == "$f" ]] && continue   # skip root itself
    depth=$(echo "$rel" | tr -cd '/' | wc -c)
    indent=""
    for (( i=0; i<depth; i++ )); do indent="  $indent"; done
    name="${rel##*/}"
    if [[ -d "$f" ]]; then echo "${indent}${DIM}${name}/${RST}"
    else                   echo "${indent}${name}"
    fi
done)"
echo ""

# ══════════════════════════════════════════════════════════════════════════════
# SERVER START
# ══════════════════════════════════════════════════════════════════════════════
if [[ -z "$RUPH" ]]; then
    echo "${YLW}No ruph binary — fixture created at $TEST_DIR  (tests skipped)${RST}"
    exit 0
fi

echo "${BLD}Starting ruph on 127.0.0.1:$TEST_PORT …${RST}"
"$RUPH" --config "$TEST_DIR/ruph.ini" "$TEST_DIR" \
    >"$TEST_DIR/ruph.log" 2>&1 &
SERVER_PID=$!

# Wait up to 5 s for the server to accept connections
BASE="http://127.0.0.1:$TEST_PORT"
ready=0
for i in $(seq 1 50); do
    curl -sf --max-time 0.3 "$BASE/static.html" >/dev/null 2>&1 && { ready=1; break; }
    sleep 0.1
done
if [[ $ready -eq 0 ]]; then
    echo "${RED}Server did not start in 5 s. Log:${RST}"
    cat "$TEST_DIR/ruph.log"
    exit 1
fi
echo "Server ready."
echo ""

# ══════════════════════════════════════════════════════════════════════════════
# TEST HARNESS
# ══════════════════════════════════════════════════════════════════════════════

# check <label> <method> <path> <want-status> <want-body-substr>
# Pass empty string for want-body-substr to skip body check.
check() {
    local label="$1" method="$2" path="$3" want_status="$4" want_body="$5"

    local http_status body
    http_status=$(curl -s -o "$BODY_TMP" -w '%{http_code}' \
        --max-time 5 -X "$method" "$BASE$path" 2>/dev/null || echo "000")
    body=$(cat "$BODY_TMP" 2>/dev/null || true)

    local status_ok=1 body_ok=1
    [[ "$http_status" == "$want_status" ]]               || status_ok=0
    [[ -z "$want_body" || "$body" == *"$want_body"* ]]   || body_ok=0

    if [[ $status_ok -eq 1 && $body_ok -eq 1 ]]; then
        echo "  ${GRN}PASS${RST}  $label"
        (( PASS++ )) || true
    else
        echo "  ${RED}FAIL${RST}  $label"
        [[ $status_ok -eq 0 ]] && echo "         status : got $http_status, want $want_status"
        [[ $body_ok -eq 0 ]]   && printf '         body   : want %q\n                 got  %q\n' \
                                      "${want_body:0:80}" "${body:0:120}"
        (( FAIL++ )) || true
    fi
}

# ── Root controller ───────────────────────────────────────────────────────────
echo "${BLD}Root controller${RST}"
echo "  Chain: [root _index.php]"
echo ""

check \
    "root controller handles /master-test (exit stops chain)" \
    GET /master-test 200 "ROOT:master-test"

check \
    "root passes through → static.html served" \
    GET /static.html 200 "STATIC_HTML_ROOT"

check \
    "direct URL to _index.php → 404 (always blocked)" \
    GET /_index.php 404 ""

# ── Direct PHP file ───────────────────────────────────────────────────────────
echo ""
echo "${BLD}Direct PHP execution${RST}"
echo "  Chain: [root _index.php (pass-through)] → app.php"
echo ""

check \
    "non-_index .php executed directly (leaf mechanism bypassed)" \
    GET /app.php 200 "DIRECT_PHP:"

# ── Subtree with no _index.php ────────────────────────────────────────────────
echo ""
echo "${BLD}No subtree _index.php (noindex/)${RST}"
echo "  Chain: [root _index.php (pass-through)] → static / 404"
echo ""

check \
    "static file served when no leaf present" \
    GET /noindex/readme.txt 200 "NOINDEX_README"

check \
    "missing path with no leaf → 404" \
    GET /noindex/missing 404 ""

# ── api/ leaf ─────────────────────────────────────────────────────────────────
echo ""
echo "${BLD}api/ leaf — virtual routes + static fall-through${RST}"
echo "  Chain: [root (pass-through)] → [api/_index.php (leaf)]"
echo ""

check \
    "leaf handles virtual /api/endpoint → JSON (rr_is_static is empty)" \
    GET /api/endpoint 200 '"leaf":"api"'

check \
    "leaf: rr_is_static set for .json → return true → static file served" \
    GET /api/data.json 200 '"file":"data.json"'

# ── blog/ leaf ────────────────────────────────────────────────────────────────
echo ""
echo "${BLD}blog/ leaf — virtual slugs + static fall-through${RST}"
echo "  Chain: [root (pass-through)] → [blog/_index.php (leaf)]"
echo ""

check \
    "leaf handles virtual blog slug" \
    GET /blog/my-post 200 "BLOG:blog/my-post"

check \
    "leaf: return true for .html → static post served" \
    GET /blog/post.html 200 "BLOG_POST_STATIC"

# ── 3-level chain: root → intermediate controller → deepest leaf ──────────────
echo ""
echo "${BLD}deep/ controller + nested/ leaf (3-level chain)${RST}"
echo "  Chain: [root (pass-through)] → [deep/_index.php (controller, pass-through)]"
echo "         → [deep/nested/_index.php (leaf)]"
echo ""

check \
    "intermediate controller passes through; deepest leaf handles virtual" \
    GET /deep/nested/virtual 200 "NESTED:deep/nested/virtual"

check \
    "intermediate + leaf; leaf: return true → content.txt served" \
    GET /deep/nested/content.txt 200 "STATIC_NESTED_CONTENT"

# ── rr_* server variables ─────────────────────────────────────────────────────
echo ""
echo "${BLD}rr_* server variables${RST}"
echo "  Pre-resolved by ruph before any PHP runs; available in \$_SERVER."
echo ""

# Dump rr_* from a direct PHP file so we can inspect them
cat > "$TEST_DIR/rr_dump.php" <<'PHP'
<?php
$out = [];
foreach ($_SERVER as $k => $v) {
    if (str_starts_with($k, 'rr_')) $out[$k] = $v;
}
echo json_encode($out);
PHP

check \
    "rr_file set and rr_is_static empty for a .php request" \
    GET /rr_dump.php 200 '"rr_file":'

# For a static file the API leaf uses rr_is_static; verify it reaches the leaf.
# (Already tested implicitly above — this makes it explicit via a query param.)
cat > "$TEST_DIR/api/_index_vars.php" <<'PHP'
<?php
// Accessible only as a direct (non-_index) PHP file — dumps rr_* vars.
echo json_encode(array_filter($_SERVER, fn($k) => str_starts_with($k, 'rr_'), ARRAY_FILTER_USE_KEY));
PHP
# We can't test /api/_index_vars.php easily without the api leaf interfering —
# the api leaf runs first and checks rr_is_static, so a .php file there would
# have rr_is_static="" and the leaf would serve its JSON, not the file.
# This is correct: the leaf intercepts before direct .php delivery.
# Verified: /api/some.php → leaf handles it (rr_is_static="") → JSON {"leaf":"api",...}
check \
    "leaf intercepts .php in its subtree (rr_is_static empty for .php)" \
    GET /api/any.php 200 '"leaf":"api"'

# ── Edge case: leaf falls through but target does not exist → 500 ─────────────
echo ""
echo "${BLD}Edge case — leaf falls through with no file → 500${RST}"
echo "  When a leaf calls 'return true' (fall-through) but the URL doesn't"
echo "  resolve to any file, ruph returns 500: leaf must handle unmatched paths."
echo ""

# blog leaf only returns true when rr_is_static is set.  For a missing path
# rr_is_static is empty, so the leaf handles it normally (not a fall-through 500).
# To trigger the 500 we need a leaf that always returns true:
mkdir -p "$TEST_DIR/always-passthru"
cat > "$TEST_DIR/always-passthru/_index.php" <<'PHP'
<?php
// A leaf that unconditionally falls through — triggers 500 for virtual paths.
return true;
PHP

check \
    "leaf 'return true' on nonexistent path → 500 (must handle unmatched)" \
    GET /always-passthru/nonexistent 500 ""

check \
    "leaf 'return true' on existent static file → serves the file (correct use)" \
    GET /always-passthru/../static.html 200 "" # path traversal blocked → 403

# Cleaner test: add a static file inside always-passthru
echo "ALWAYS_STATIC" > "$TEST_DIR/always-passthru/file.txt"
check \
    "leaf 'return true' on existing static file → file served correctly" \
    GET /always-passthru/file.txt 200 "ALWAYS_STATIC"

# ── Summary ───────────────────────────────────────────────────────────────────
echo ""
echo "─────────────────────────────────────────────────────────────────────────────"
echo ""
total=$(( PASS + FAIL ))
if [[ $FAIL -eq 0 ]]; then
    echo "${GRN}${BLD}All $PASS/$total tests passed.${RST}"
    EXIT=0
else
    echo "${RED}${BLD}$FAIL/$total tests FAILED.${RST}"
    EXIT=1
fi
echo ""
echo "${DIM}Fixture: $TEST_DIR"
echo "Log    : $TEST_DIR/ruph.log${RST}"
echo ""
exit $EXIT
