# PHP Processing Modes

## Modes

| Mode | INI value | Behavior |
|---|---|---|
| **AST** | `ast` | Tree-sitter parser, no external binary needed |
| **CGI** | `cgi` | External php-cgi binary via FastCGI-style invocation |
| **Embedded** | `embedded` | Regex-based processor (limited PHP support) |
| **Auto** | `auto` | Tries AST -> embedded -> CGI in order |

## Configuration

In `ruph.ini` under `[php.*]`:

```ini
[php.*]
processor = ast
binary = /usr/local/bin/php-cgi
```

`binary` is only needed for CGI mode. AST mode processes PHP in-process using
tree-sitter without spawning external processes.

## $_SERVER injection

Both AST and CGI modes inject standard CGI variables plus RUPH_* metrics into
`$_SERVER`. The injection happens in `web_server.rs`:

- `build_server_vars()` — standard CGI vars (REQUEST_URI, HTTP_HOST, etc.)
- `build_server_vars_with_addr()` — adds RUPH_* stats when ServerStats is available

See [rate-limiting.md](rate-limiting.md) for the full list of RUPH_* variables.

## Master _index.php

ruph runs `/var/www/live/ruph_root/_index.php` on every request before dispatching
to domain-specific docroots. This file:

1. Enforces rate limiting using RUPH_* vars
2. Blocks path traversal and CMS probes
3. Defines shared utility functions (validate_search_query, stream_grok_response)
