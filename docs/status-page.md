# Status Page

## Overview

ruph serves an HTML dashboard at a configurable path showing live server metrics.

## Configuration

In `ruph.ini` under `[server]`:

```ini
status_page = /status
```

Set to any URL path. Omit or leave empty to disable.

## Metrics displayed

- Active connections
- Requests/sec (10s and 60s averages)
- Total requests since start
- Uptime
- Unique IPs seen
- Top 50 IPs by request count

## Implementation

- `ServerStats` in `status.rs` uses lock-free atomics for QPS and connection counts
- QPS uses a 60-slot ring buffer keyed by elapsed second — no allocations per request
- Per-IP data uses a mutex-guarded HashMap (one lock per request)
- `render_status_page()` generates self-contained HTML with inline CSS (dark theme)
