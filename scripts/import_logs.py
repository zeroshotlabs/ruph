#!/usr/bin/env python3
"""Import ruph text log files into the log_full SQLite database.

Usage:
    python3 import_logs.py /path/to/requests.db /path/to/*.log
    python3 import_logs.py --purge /path/to/requests.db /path/to/*.log

Formats handled:
    HH:MM:SS  INFO [status] [domain] ip:port METHOD path
    HH:MM:SS  INFO [status] S [domain] ip:port METHOD path   (TLS)
    HH:MM:SS  INFO [status] - [domain] ip:port METHOD path   (plain)
    INFO [status] [domain] ip:port METHOD path                (no timestamp)

Lines not matching (PHP debug, errors, etc.) are silently skipped.
"""

import sys
import os
import sqlite3
from datetime import datetime, date

BATCH_SIZE = 50000


def ensure_schema(conn):
    """Create the requests table if it doesn't exist (matches request_log.rs)."""
    conn.executescript("""
        CREATE TABLE IF NOT EXISTS requests (
            id              INTEGER PRIMARY KEY,
            ts              TEXT    NOT NULL,
            ts_epoch_ms     INTEGER NOT NULL,
            ip              TEXT    NOT NULL,
            port            INTEGER NOT NULL,
            method          TEXT    NOT NULL,
            host            TEXT    NOT NULL,
            path            TEXT    NOT NULL,
            query           TEXT,
            protocol        TEXT,
            status          INTEGER,
            response_size   INTEGER,
            duration_us     INTEGER,
            tls             INTEGER NOT NULL DEFAULT 0,
            sni             TEXT,
            http_version    TEXT,
            request_headers TEXT,
            user_agent      TEXT,
            referer         TEXT,
            accept          TEXT,
            accept_language TEXT,
            accept_encoding TEXT,
            content_type    TEXT,
            content_length  INTEGER,
            cookie          TEXT,
            authorization   TEXT,
            x_forwarded_for TEXT,
            origin          TEXT,
            response_headers TEXT,
            vhost           TEXT,
            geo_country     TEXT,
            geo_city        TEXT,
            geo_region      TEXT,
            geo_lat         REAL,
            geo_lon         REAL,
            asn             TEXT,
            asn_org         TEXT,
            bot_flag        INTEGER,
            bot_name        TEXT,
            abuse_score     REAL,
            notes           TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_req_ts      ON requests(ts);
        CREATE INDEX IF NOT EXISTS idx_req_epoch   ON requests(ts_epoch_ms);
        CREATE INDEX IF NOT EXISTS idx_req_ip      ON requests(ip);
        CREATE INDEX IF NOT EXISTS idx_req_host    ON requests(host);
        CREATE INDEX IF NOT EXISTS idx_req_status  ON requests(status);
        CREATE INDEX IF NOT EXISTS idx_req_method  ON requests(method);
        CREATE INDEX IF NOT EXISTS idx_req_path    ON requests(path);
        CREATE INDEX IF NOT EXISTS idx_req_ua      ON requests(user_agent);
        CREATE INDEX IF NOT EXISTS idx_req_tls     ON requests(tls);
    """)


INSERT_SQL = """
    INSERT INTO requests (
        ts, ts_epoch_ms, ip, port, method, host, path, query,
        protocol, status, tls, vhost, notes
    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
"""


def parse_log_file(filepath, file_date):
    """Yield (row_tuple) for each request line in the file.
    Uses string splitting instead of regex for speed."""
    source_note = f"imported:{os.path.basename(filepath)}"
    # Pre-compute midnight epoch for this file date, add seconds per line
    midnight_dt = datetime.strptime(file_date, "%Y-%m-%d")
    midnight_epoch_ms = int(midnight_dt.timestamp() * 1000)

    with open(filepath, 'r', errors='replace') as f:
        for line in f:
            # Fast reject: request lines always contain ' INFO ['
            info_pos = line.find(' INFO [')
            if info_pos == -1:
                continue

            # Extract time from start (HH:MM:SS before INFO)
            before_info = line[:info_pos].strip()
            if before_info and len(before_info) == 8 and before_info[2] == ':':
                time_str = before_info
                h, m, s = int(time_str[0:2]), int(time_str[3:5]), int(time_str[6:8])
                ts_str = f"{file_date}T{time_str}"
                epoch_ms = midnight_epoch_ms + (h * 3600 + m * 60 + s) * 1000
            else:
                ts_str = f"{file_date}T00:00:00"
                epoch_ms = midnight_epoch_ms

            # Parse after INFO: [status] [S|-] [domain] ip:port METHOD path
            rest = line[info_pos + 6:].strip()  # skip ' INFO '

            # [status]
            if not rest.startswith('['):
                continue
            bracket_end = rest.find(']', 1)
            if bracket_end == -1:
                continue
            status_str = rest[1:bracket_end]
            if not status_str.isdigit():
                continue
            status = int(status_str)

            rest = rest[bracket_end + 1:].lstrip()

            # Optional proto flag: S or -
            is_tls = 0
            if rest and rest[0] in ('S', '-'):
                if len(rest) > 1 and rest[1] == ' ':
                    is_tls = 1 if rest[0] == 'S' else 0
                    rest = rest[2:].lstrip()

            # [domain]
            if not rest.startswith('['):
                continue
            bracket_end = rest.find(']', 1)
            if bracket_end == -1:
                continue
            domain = rest[1:bracket_end]

            rest = rest[bracket_end + 1:].lstrip()

            # ip:port METHOD path
            # Find the colon separating IP from port (last colon before first space)
            space_pos = rest.find(' ')
            if space_pos == -1:
                continue
            addr = rest[:space_pos]
            colon_pos = addr.rfind(':')
            if colon_pos == -1:
                continue
            ip = addr[:colon_pos]
            port_str = addr[colon_pos + 1:]
            if not port_str.isdigit():
                continue
            port = int(port_str)

            rest = rest[space_pos + 1:].lstrip()

            # METHOD path
            space_pos = rest.find(' ')
            if space_pos == -1:
                continue
            method = rest[:space_pos]
            full_path = rest[space_pos + 1:].split()[0] if rest[space_pos + 1:] else ''

            # Split path and query
            qpos = full_path.find('?')
            if qpos != -1:
                path, query = full_path[:qpos], full_path[qpos + 1:]
            else:
                path, query = full_path, None

            protocol = 'https' if is_tls else 'http'

            yield (
                ts_str, epoch_ms, ip, port, method,
                domain, path, query, protocol, status,
                is_tls, domain, source_note,
            )


def import_file(conn, filepath):
    """Import a single log file, return count of rows inserted."""
    # Use file modification date as the date component
    mtime = os.path.getmtime(filepath)
    file_date = date.fromtimestamp(mtime).isoformat()

    batch = []
    total = 0

    for row in parse_log_file(filepath, file_date):
        batch.append(row)
        if len(batch) >= BATCH_SIZE:
            conn.executemany(INSERT_SQL, batch)
            conn.commit()
            total += len(batch)
            batch.clear()

    if batch:
        conn.executemany(INSERT_SQL, batch)
        conn.commit()
        total += len(batch)

    return total


def main():
    args = sys.argv[1:]
    purge = False
    if '--purge' in args:
        purge = True
        args.remove('--purge')

    if len(args) < 2:
        print("Usage: python3 import_logs.py [--purge] <db_path> <log_file> [log_file ...]")
        sys.exit(1)

    db_path = args[0]
    log_files = args[1:]

    conn = sqlite3.connect(db_path)
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA synchronous=NORMAL")
    ensure_schema(conn)

    grand_total = 0
    for filepath in log_files:
        if not os.path.isfile(filepath):
            print(f"  skip (not a file): {filepath}")
            continue

        size_mb = os.path.getsize(filepath) / (1024 * 1024)
        print(f"  importing {filepath} ({size_mb:.1f} MB) ...", end='', flush=True)
        count = import_file(conn, filepath)
        print(f" {count:,} rows")
        grand_total += count

        if purge and count > 0:
            # Truncate the log file (keep the file handle for the running server)
            with open(filepath, 'w') as f:
                pass
            print(f"  purged {filepath}")

    conn.close()
    print(f"\nDone: {grand_total:,} total rows imported into {db_path}")


if __name__ == '__main__':
    main()
