# SSL Certificate Management with ruph

ruph has a built-in ACME client with an embedded DNS server for DNS-01 challenge validation. This allows issuing and renewing certificates — including wildcards — without stopping the running server or opening port 80.

## How It Works

1. ruph embeds a minimal authoritative DNS server (UDP 53)
2. For each domain, a stable CNAME points `_acme-challenge.<domain>` to a subdomain in ruph's DNS zone
3. When issuing a cert, ruph sets a TXT record on that subdomain, Let's Encrypt validates it, and the cert is issued

## One-Time Setup

### 1. DNS Zone Delegation

Choose a subdomain for ruph's DNS zone (e.g., `acme.libzsl.com`). Add these two records at the parent domain's DNS provider:

```
acme.libzsl.com.  A   <your-server-ip>
acme.libzsl.com.  NS  acme.libzsl.com.
```

### 2. Configuration

Create or edit `~/.ruph/ruph.ini`:

```ini
[acme]
zone = acme.libzsl.com
email = hostmaster@libzsl.com
```

Optional: set `dns_bind = 0.0.0.0:53` (this is the default).

## Per-Domain Setup

### Single Domain

```
ruph --new-cert example.com
```

On first run for a new domain, ruph prints the CNAME record to add:

```
_acme-challenge.example.com  CNAME  <uuid>.acme.libzsl.com
```

Add this CNAME at whatever DNS provider manages `example.com`. This is a one-time step — the UUID is stable and persisted in `~/.ruph/ssl/acme_domains.json`.

Once the CNAME is in place, run the command again (or it will auto-detect propagation and continue).

### Wildcard Domain

```
ruph --new-cert "*.example.com"
```

The CNAME goes on the base domain:

```
_acme-challenge.example.com  CNAME  <uuid>.acme.libzsl.com
```

Wildcards and bare domains share the same CNAME since both validate via `_acme-challenge.<base>`.

## Renewing Certificates

Re-run the same `--new-cert` command. The CNAME is already in place, so it proceeds immediately:

```
ruph --new-cert example.com
ruph --new-cert "*.example.com"
```

## Checking Certificate Status

List all certificates and their expiry dates:

```
ruph --list-certs
```

Warnings are printed at server startup for any certificate expiring within 30 days.

## Batch Renewal

Renew multiple domains in sequence:

```bash
for domain in example.com "*.example.org" another.net; do
  echo "=== $domain ==="
  ruph --new-cert "$domain"
done
```

## Legacy Mode (HTTP-01)

If no `[acme]` section is configured, `--new-cert` falls back to HTTP-01 validation. This requires port 80 to be free (server must be stopped) and does not support wildcards:

```
ruph --new-cert email@example.com,example.com
```

## File Layout

```
~/.ruph/
  ruph.ini                    # Config file with [acme] section
  ssl/
    acme_account.json         # Let's Encrypt account credentials
    acme_domains.json         # Domain -> UUID subdomain mapping
    example.com/
      fullchain.pem           # Certificate chain
      privkey.pem             # Private key
    *.example.com/
      fullchain.pem
      privkey.pem
```

## Troubleshooting

**Port 53 already in use**: Stop any local DNS resolver (e.g., `systemd-resolved`) or set `dns_bind` to a different address in the config. On Linux without root, use: `setcap cap_net_bind_service=+ep /path/to/ruph`

**CNAME not propagating**: Check with `dig +short CNAME _acme-challenge.example.com @8.8.8.8`. DNS propagation can take a few minutes. ruph will retry for up to 10 minutes.

**"No such authorization" error**: Usually a stale ACME order from a previous failed attempt. Just re-run the command.

**"CSR does not specify same identifiers"**: The domain in the command must match what's in the ACME order. For wildcards, use `"*.example.com"` (quoted to prevent shell glob expansion).
