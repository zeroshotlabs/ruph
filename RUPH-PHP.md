# Ruph PHP — Complete Reference

Ruph's built-in PHP interpreter. No external PHP binary required.

---

## Language

### Tags

```php
<?php ... ?>        // Standard tag
<?= $expr ?>        // Short echo tag (equivalent to <?php echo $expr; ?>)
```

Inline HTML outside PHP tags is passed through as-is.

### Variables

```php
$name = "value";              // Assignment
$arr['key'] = "value";        // Array subscript assignment
$arr[] = "appended";          // Array append
$$var                          // Not supported
```

### Types

| Type | Literal | Notes |
|------|---------|-------|
| String | `'single'`, `"double"`, `"interp $var"` | Escape sequences in double quotes |
| Int | `42`, `-1`, `0` | 64-bit signed |
| Float | `3.14`, `-0.5` | 64-bit double |
| Bool | `true`, `false` | Case-insensitive |
| Null | `null` | Case-insensitive |
| Array | `[1, 2]`, `['k' => 'v']` | Indexed and associative |

### Operators

#### Arithmetic
`+`  `-`  `*`  `/`  `%`  `**`

#### String
`.` (concatenation)

#### Assignment
`=`  `+=`  `-=`  `*=`  `/=`  `.=`  `??=`

#### Comparison
`==`  `!=`  `<>`  `===`  `!==`  `<`  `>`  `<=`  `>=`

#### Logical
`&&`  `||`  `!`  `and`  `or` — short-circuit evaluation

#### Null
`??` (null coalescing) — returns left if non-null, else right

#### Ternary
`$a ? $b : $c` — standard ternary
`$a ?: $b` — short ternary (returns `$a` if truthy)

#### Increment/Decrement
`++$i`  `$i++`  `--$i`  `$i--`

#### Bitwise
`&`  `|`  `^`  `~`  `<<`  `>>`

#### Type Cast
`(int)`  `(integer)`  `(float)`  `(double)`  `(string)`  `(bool)`  `(boolean)`  `(array)`

### Control Flow

```php
// if / elseif / else
if ($condition) {
    ...
} elseif ($other) {
    ...
} else {
    ...
}

// while
while ($condition) {
    ...
}

// for
for ($i = 0; $i < 10; $i++) {
    ...
}

// foreach
foreach ($array as $value) { ... }
foreach ($array as $key => $value) { ... }

// switch
switch ($value) {
    case 'a':
        ...
        break;
    case 'b':
        ...
        break;
    default:
        ...
}

// break / continue
break;
continue;

// return (from function or script — sets response body override)
return $value;

// exit / die (terminates script — signals ruph: request handled)
exit;
exit(0);
exit("message");   // outputs message then exits
die;
die("message");
```

### Functions

```php
// Definition
function greet($name, $greeting = "Hello") {
    return "$greeting, $name!";
}

// Call
$result = greet("World");
$result = greet("World", "Hi");
```

User-defined functions have local scope. Parameters support default values.

### Include / Require

```php
include 'header.php';
include_once 'config.php';
require 'functions.php';
require_once 'db.php';
```

Paths are resolved relative to the current template, or absolute from docroot if starting with `/`. Path traversal is blocked.

### Superglobals

| Variable | Contents |
|----------|----------|
| `$_SERVER` | Request info: `REQUEST_URI`, `REQUEST_METHOD`, `HTTP_HOST`, `DOCUMENT_ROOT`, `rr_file`, `rr_dir`, `rr_index`, `rr_leaf_idx`, `rr_mime`, all `HTTP_*` headers |
| `$_GET` | Query string parameters |
| `$_POST` | POST body parameters |
| `$_REQUEST` | Merged `$_GET` + `$_POST` |

### Constants

#### Pre-defined

| Constant | Value |
|----------|-------|
| `PATHINFO_DIRNAME` | `1` |
| `PATHINFO_BASENAME` | `2` |
| `PATHINFO_EXTENSION` | `4` |
| `PATHINFO_FILENAME` | `8` |
| `PHP_EOL` | `"\n"` |
| `PHP_INT_MAX` | `9223372036854775807` |
| `PHP_INT_MIN` | `-9223372036854775808` |
| `DIRECTORY_SEPARATOR` | `"/"` |
| `PHP_SAPI` | `"ruph-ast"` |
| `PHP_VERSION` | `"8.4.0-ruph"` |
| `TRUE` | `true` |
| `FALSE` | `false` |
| `NULL` | `null` |

#### User-defined

```php
define('SITE_NAME', 'My Site');
if (defined('SITE_NAME')) { ... }
echo constant('SITE_NAME');
```

---

## Functions — Complete List

### String (30)

| Function | Signature | Returns |
|----------|-----------|---------|
| `strlen` | `strlen($s)` | int — byte length |
| `strtolower` | `strtolower($s)` | string |
| `strtoupper` | `strtoupper($s)` | string |
| `trim` | `trim($s [, $chars])` | string |
| `ltrim` | `ltrim($s)` | string |
| `rtrim` / `chop` | `rtrim($s)` | string |
| `substr` | `substr($s, $start [, $len])` | string |
| `str_replace` | `str_replace($search, $replace, $subject)` | string |
| `str_contains` | `str_contains($haystack, $needle)` | bool |
| `str_starts_with` | `str_starts_with($haystack, $needle)` | bool |
| `str_ends_with` | `str_ends_with($haystack, $needle)` | bool |
| `strpos` | `strpos($haystack, $needle)` | int \| false |
| `stripos` | `stripos($haystack, $needle)` | int \| false |
| `strrpos` | `strrpos($haystack, $needle)` | int \| false |
| `explode` | `explode($delim, $string [, $limit])` | array |
| `implode` / `join` | `implode($glue, $array)` | string |
| `sprintf` | `sprintf($fmt, ...)` | string — supports `%s`, `%d`, `%f`, `%%` |
| `nl2br` | `nl2br($s)` | string |
| `htmlspecialchars` | `htmlspecialchars($s)` | string |
| `htmlspecialchars_decode` | `htmlspecialchars_decode($s)` | string |
| `htmlentities` | `htmlentities($s)` | string — same as htmlspecialchars |
| `urlencode` | `urlencode($s)` | string |
| `urldecode` | `urldecode($s)` | string |
| `rawurlencode` | `rawurlencode($s)` | string |
| `rawurldecode` | `rawurldecode($s)` | string |
| `ucfirst` | `ucfirst($s)` | string |
| `lcfirst` | `lcfirst($s)` | string |
| `str_repeat` | `str_repeat($s, $n)` | string |
| `str_pad` | `str_pad($input, $length [, $pad])` | string — right-pads |
| `number_format` | `number_format($num [, $decimals])` | string |
| `md5` | `md5($s)` | string — hash (non-standard impl) |

### Array (20)

| Function | Signature | Returns |
|----------|-----------|---------|
| `count` / `sizeof` | `count($arr)` | int |
| `array_keys` | `array_keys($arr)` | array |
| `array_values` | `array_values($arr)` | array |
| `in_array` | `in_array($needle, $arr)` | bool |
| `array_key_exists` | `array_key_exists($key, $arr)` | bool |
| `array_merge` | `array_merge($a, $b, ...)` | array |
| `array_push` | `array_push($arr, $val, ...)` | int — count |
| `array_pop` | `array_pop($arr)` | mixed — last value |
| `array_slice` | `array_slice($arr, $offset [, $length])` | array |
| `array_reverse` | `array_reverse($arr)` | array |
| `array_unique` | `array_unique($arr)` | array |
| `array_map` | `array_map(null, $arr)` | array — callback not supported |
| `range` | `range($start, $end [, $step])` | array |
| `compact` | `compact($name1, $name2, ...)` | array — builds from variable names |
| `extract` | `extract($arr)` | null — sets variables from array keys |
| `sort` | `sort($arr)` | true — stub (no by-reference mutation) |
| `rsort` | `rsort($arr)` | true — stub |
| `asort` | `asort($arr)` | true — stub |
| `arsort` | `arsort($arr)` | true — stub |
| `ksort` | `ksort($arr)` | true — stub |

### Type (12)

| Function | Signature | Returns |
|----------|-----------|---------|
| `isset` | `isset($a [, $b, ...])` | bool — true if all non-null |
| `empty` | `empty($a)` | bool — true if null/false/0/""/[] |
| `is_null` | `is_null($a)` | bool |
| `is_array` | `is_array($a)` | bool |
| `is_string` | `is_string($a)` | bool |
| `is_numeric` | `is_numeric($a)` | bool |
| `is_int` / `is_integer` / `is_long` | `is_int($a)` | bool |
| `is_bool` | `is_bool($a)` | bool |
| `gettype` | `gettype($a)` | string — "integer", "string", etc. |
| `intval` | `intval($a)` | int |
| `floatval` / `doubleval` | `floatval($a)` | float |
| `strval` | `strval($a)` | string |
| `boolval` | `boolval($a)` | bool |

### JSON (2)

| Function | Signature | Returns |
|----------|-----------|---------|
| `json_encode` | `json_encode($value)` | string |
| `json_decode` | `json_decode($json [, $assoc])` | mixed — arrays when `$assoc=true` |

### Filesystem (12)

| Function | Signature | Returns |
|----------|-----------|---------|
| `file_exists` | `file_exists($path)` | bool |
| `is_file` | `is_file($path)` | bool |
| `is_dir` | `is_dir($path)` | bool |
| `readfile` | `readfile($path)` | int — bytes output |
| `file_get_contents` | `file_get_contents($path_or_url)` | string — works with HTTP URLs |
| `file_put_contents` | `file_put_contents($path, $data)` | int — bytes written |
| `filesize` | `filesize($path)` | int \| false |
| `dirname` | `dirname($path)` | string |
| `basename` | `basename($path)` | string |
| `pathinfo` | `pathinfo($path [, $flag])` | array \| string — supports PATHINFO_* flags |
| `realpath` | `realpath($path)` | string \| false |
| `glob` | `glob($pattern)` | array — simplified directory listing |

### Date/Time (4)

| Function | Signature | Returns |
|----------|-----------|---------|
| `date` | `date($format)` | string — Y-m-d H:i:s format |
| `time` | `time()` | int — Unix timestamp |
| `microtime` | `microtime([$as_float])` | string \| float |
| `strtotime` | `strtotime($str)` | int \| false — supports "now", ISO dates |

### Math (7)

| Function | Signature | Returns |
|----------|-----------|---------|
| `abs` | `abs($n)` | float |
| `ceil` | `ceil($n)` | float |
| `floor` | `floor($n)` | float |
| `round` | `round($n [, $precision])` | float |
| `max` | `max($a, $b, ...)` or `max($arr)` | float |
| `min` | `min($a, $b, ...)` or `min($arr)` | float |
| `rand` / `mt_rand` | `rand([$min, $max])` | int |

### Regex (3)

| Function | Signature | Returns |
|----------|-----------|---------|
| `preg_match` | `preg_match($pattern, $subject)` | int (0 or 1) |
| `preg_replace` | `preg_replace($pattern, $replacement, $subject)` | string |
| `preg_split` | `preg_split($pattern, $subject)` | array |

Patterns use PHP delimiters: `/pattern/flags`. The `i`, `m`, `s` flags are passed to the Rust regex engine.

### HTTP / Output (7)

| Function | Signature | Returns |
|----------|-----------|---------|
| `header` | `header('Name: value')` | null — sets response header |
| `http_response_code` | `http_response_code($code)` | int — sets/returns status |
| `setcookie` | `setcookie($name, $value)` | true — sets Set-Cookie header |
| `echo` | `echo $expr` | — outputs to response |
| `print_r` | `print_r($val [, $return])` | string \| true |
| `var_dump` | `var_dump($val, ...)` | null — outputs type info |
| `error_log` | `error_log($msg)` | true — writes to ruph log |

### Output Buffering (2)

| Function | Signature | Returns |
|----------|-----------|---------|
| `ob_start` | `ob_start()` | null — starts capturing output |
| `ob_get_clean` | `ob_get_clean()` | string — returns and clears buffer |

### Ruph Extensions (4)

| Function | Signature | Returns |
|----------|-----------|---------|
| `exe` | `exe($path)` | string — execute `_index.php` in dir or a PHP file |
| `render` | `render($template [, $data])` | string — render template with data |
| `response` | `response($status [, $headers [, $body]])` | null — set full response |
| `http_request` | `http_request($method, $url [, $headers [, $body]])` | string — HTTP client |

### Introspection / Misc (10)

| Function | Signature | Returns |
|----------|-----------|---------|
| `phpversion` | `phpversion()` | `"8.4.0-ast"` |
| `php_uname` | `php_uname()` | string — OS name |
| `php_sapi_name` | `php_sapi_name()` | `"ruph-ast"` |
| `function_exists` | `function_exists($name)` | bool |
| `define` | `define($name, $value)` | true |
| `defined` | `defined($name)` | bool |
| `constant` | `constant($name)` | mixed |
| `session_start` | `session_start()` | true — stub |
| `session_destroy` | `session_destroy()` | true — stub |
| `sleep` / `usleep` | `sleep($s)` | 0 — no-op |

---

## Not Supported

These PHP features are **not** available in ruph's built-in interpreter:

- Classes, objects, interfaces, traits (`class`, `new`, `->`, `::`, `implements`)
- Namespaces (`namespace`, `use`)
- Anonymous functions / closures (`function() { }`, `fn() =>`)
- Generators (`yield`, `yield from`)
- Exceptions (`try`, `catch`, `throw`, `finally`)
- References (`&$var`)
- Variable variables (`$$var`)
- List assignment (`list($a, $b) = ...`, `[$a, $b] = ...`)
- Match expressions (`match ($x) { ... }`)
- Enums
- Named arguments (`foo(name: $value)`)
- Fibers
- Attributes (`#[...]`)
- PHP extensions (PDO, curl, mbstring, GD, etc.)
- `$_SESSION` persistence (stubs only)
- `$_COOKIE`, `$_FILES`, `$_ENV` superglobals

For these features, use `processor = cgi` in `ruph.ini` to route through an external PHP binary.
