# PHP Features to Implement in Rust

Unsupported PHP functions/features discovered while porting `trender.php` to ruph.
Each item was worked around in PHP (foreach loops, regex XML parsing, etc.) but should
be implemented natively in the AST interpreter for performance and correctness.

## Priority 1 — Array Higher-Order Functions

These are used constantly in real PHP code. Without them, every script needs verbose foreach rewrites.

| PHP Function | Signature | Rust Crate / Approach |
|---|---|---|
| `array_map($callback, $array)` | Applies callback (closure, arrow fn, or string name) to each element | Inline in interpreter — resolve callback to `PhpFunction` or builtin, iterate `Vec<PhpValue>` |
| `array_filter($array, $callback)` | Keeps elements where callback returns truthy | Same as above, `.retain()` or `.into_iter().filter()` |
| `usort(&$array, $comparator)` | In-place sort using comparator callback | `Vec::sort_by()` with async callback evaluation |
| `array_walk(&$array, $callback)` | Mutate each element in-place via callback | Iterate with mutable refs |

### Prerequisite: Closure / Arrow Function Support

All of the above depend on being able to evaluate closures and arrow functions as callable values.

| Feature | PHP Syntax | Implementation Notes |
|---|---|---|
| Closure with `use` | `function($x) use ($y) { ... }` | Capture listed variables from current scope into a `PhpValue::Closure` variant |
| Arrow function | `fn($x) => expr` | Sugar for single-expression closure; implicit capture of outer scope |
| String callback | `'function_name'` | Already partially works for builtins; extend to user-defined functions |
| Spaceship operator | `$a <=> $b` | Returns -1/0/1; needed for `usort` comparators. Add to binary_expression handler |

## Priority 2 — HTTP Client (curl)

Currently worked around with `file_get_contents()` for simple GETs, but curl is needed
for POST, custom headers, auth tokens, timeouts, and response metadata.

| PHP Function | What It Does | Rust Crate |
|---|---|---|
| `curl_init($url)` | Create handle | `reqwest` (async, already using tokio) |
| `curl_setopt_array($ch, $opts)` | Set options (headers, method, timeout, follow redirects, etc.) | `reqwest::ClientBuilder` / `RequestBuilder` |
| `curl_exec($ch)` | Execute request, return body | `client.send().await` |
| `curl_getinfo($ch, $opt)` | Get status code, content-type, etc. | `Response::status()`, `Response::headers()` |
| `curl_close($ch)` | Free handle | Drop (automatic) |

**Approach:** Introduce a `PhpValue::CurlHandle` variant wrapping a struct that accumulates
options, then on `curl_exec` build and fire a `reqwest::Request`. Map common `CURLOPT_*`
constants to reqwest equivalents.

| CURLOPT Constant | reqwest Equivalent |
|---|---|
| `CURLOPT_RETURNTRANSFER` | Always true (return body as string) |
| `CURLOPT_HTTPHEADER` | `.headers(HeaderMap)` |
| `CURLOPT_TIMEOUT` | `.timeout(Duration::from_secs(n))` |
| `CURLOPT_FOLLOWLOCATION` | `redirect::Policy::limited(n)` |
| `CURLOPT_MAXREDIRS` | `redirect::Policy::limited(n)` |
| `CURLOPT_USERAGENT` | `.header("User-Agent", val)` |
| `CURLOPT_POST` / `CURLOPT_POSTFIELDS` | `.method(POST).body(data)` |
| `CURLINFO_HTTP_CODE` | `response.status().as_u16()` |
| `CURLINFO_CONTENT_TYPE` | `response.headers().get("content-type")` |

## Priority 3 — XML Parsing (SimpleXML)

Currently worked around with regex, which is fragile. Real XML parsing is needed for
RSS feeds, SOAP, CJ API responses, etc.

| PHP Function | What It Does | Rust Crate |
|---|---|---|
| `simplexml_load_string($xml)` | Parse XML string into traversable object | `quick-xml` or `roxmltree` |
| `$node->childName` | Access child element by name | Tree traversal on parsed DOM |
| `$node->{'hyphenated-name'}` | Access child with special chars in name | Same, key lookup |
| `(string)$node` | Get text content | `.text()` on node |
| `foreach ($node->children as $child)` | Iterate child elements | Iterator over child nodes |

**Approach:** `roxmltree` is zero-copy and fast for read-only access (perfect for SimpleXML).
Introduce `PhpValue::XmlElement` wrapping a parsed tree + node ID. Implement
`member_access_expression` and iteration for it.

## Priority 4 — Crypto / Random

| PHP Function | What It Does | Rust Crate |
|---|---|---|
| `random_bytes($length)` | Cryptographically secure random bytes | `rand::rngs::OsRng` + `rand::RngCore::fill_bytes()` |
| `bin2hex($data)` | Binary string to hex | `hex::encode()` (`hex` crate) or manual |
| `chr($byte)` | Byte value to character | `char::from(n as u8)` |
| `ord($char)` | Character to byte value | `s.bytes().next()` |
| `str_split($string, $length)` | Split string into chunks | `.as_bytes().chunks(n)` |
| `vsprintf($format, $args)` | sprintf with array of args | Extend existing `sprintf` impl to accept array |

## Priority 5 — Filesystem Extras

| PHP Function | What It Does | Rust Crate |
|---|---|---|
| `filemtime($path)` | File modification time as unix timestamp | `std::fs::metadata().modified()` |
| `mkdir($path, $mode, $recursive)` | Create directory | `std::fs::create_dir_all()` |
| `is_dir($path)` | Check if path is directory | `std::path::Path::is_dir()` — may already work |
| `@expression` | Error suppression operator | Suppress `log_error` output, return null/false on failure |

## Implementation Order Suggestion

1. **Closures + arrow functions** — unlocks array_map/filter/usort and much more
2. **Spaceship operator (`<=>`)** — trivial to add, needed for usort
3. **array_map, array_filter, usort** — immediate high-value once closures work
4. **curl via reqwest** — already have tokio; unlocks real HTTP client work
5. **SimpleXML via roxmltree** — unlocks RSS/XML APIs
6. **Crypto/random builtins** — small surface area, easy wins
7. **Filesystem extras** — filemtime, mkdir

## Rust Crate Summary

| Crate | Version | Used For |
|---|---|---|
| `reqwest` | 0.12+ | curl_* functions (HTTP client) |
| `roxmltree` | 0.20+ | simplexml_load_string (read-only XML) |
| `quick-xml` | 0.36+ | Alternative XML (if write support needed) |
| `hex` | 0.4+ | bin2hex / hex2bin |
| `rand` | 0.9+ | random_bytes, improved mt_rand |
