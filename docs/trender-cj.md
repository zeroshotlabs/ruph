# Trender — CJ Affiliate Integration

## Overview

`luxurybestdeals.com` runs the trender application, which harvests trending topics
from RSS feeds and matches them with CJ (Commission Junction) affiliate offers.

## Entry point

`/var/www/live/ruph_root/luxurybestdeals.com/_index.php` requires `trender.php`.

## How it works

1. **Topic harvesting** (`/refresh` route):
   - Fetches Slashdot RSS + PCMag feeds
   - Extracts titles, summaries, and keywords
   - Stores topics in a local cache

2. **CJ search** (on page render):
   - Extracts keywords from the topic title/summary
   - Queries `linksearch.api.cj.com/v3/link-search` for matching affiliate links
   - Scores results by keyword relevance (exact match > partial > category)
   - Displays top offers with commission info

## CJ API notes

- Always returns XML regardless of Accept header
- Key fields: `destination` (affiliate URL), `link-name`, `advertiser-name`, `sale-commission`
- Auth: `Authorization: Bearer <developer-key>` header
- Website ID is passed as `website-id` query parameter

## Routes

| Path | Description |
|---|---|
| `/` | Homepage with trending topics |
| `/p/{slug}` | Individual topic page with CJ offers |
| `/refresh` | Re-harvest topics from RSS feeds |
| `/search?q=...` | SSE search via Grok API |

## Config

`trender.php` reads `config.json` from the same directory for API keys and settings.
