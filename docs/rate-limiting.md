# Rate Limiting

## How it works

Rate limiting is split across two layers:

1. **Rust (status.rs)** — tracks per-IP request counts using a sliding-window ring buffer
2. **PHP (_index.php)** — reads the counts from `$_SERVER` and enforces thresholds

## Rust side: per-IP tracking

`ServerStats` maintains a `HashMap<IpAddr, IpWindow>` behind a mutex. Each `IpWindow`
is a 60-slot ring buffer keyed by elapsed second. On every request, the server calls
`record_request(ip)` which bumps the per-second counter and the lifetime total.

The `server_vars(ip)` method returns a `HashMap<String, String>` injected into PHP's
`$_SERVER` superglobal:

| Variable | Description |
|---|---|
| `RUPH_IP_HITS` | Total requests from this IP since server start |
| `RUPH_IP_HITS_WINDOW` | Requests from this IP in the last N seconds |
| `RUPH_RATE_WINDOW` | The window size in seconds (from config) |
| `REMOTE_IP` | The client's IP address |
| `RUPH_QPS_10` | Server-wide requests/sec (10s average) |
| `RUPH_QPS_60` | Server-wide requests/sec (60s average) |
| `RUPH_TOTAL_REQUESTS` | Total requests since server start |
| `RUPH_ACTIVE_CONNECTIONS` | Current open connections |
| `RUPH_UPTIME` | Server uptime in seconds |

## PHP side: enforcement tiers

In `/var/www/live/ruph_root/_index.php`, three tiers run on every request:

1. **Banned IPs** — hardcoded list, tarpit (sleep 30s then drop)
2. **Burst limit** — `RUPH_IP_HITS_WINDOW > 30` → HTTP 429 with `Retry-After: 5`
3. **Lifetime abuse** — `RUPH_IP_HITS > 5000` → tarpit (sleep 30s then drop)

Additionally, common CMS probe paths (wp-login, wp-admin, xmlrpc.php, .env,
phpmyadmin, administrator) are blocked with an instant 404.

## Configuration

In `ruph.ini` under `[server]`:

```ini
rate_window = 2    ; sliding window in seconds (default 2)
```

The window size is capped between 1 and 60 seconds.
