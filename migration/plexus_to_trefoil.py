#!/usr/bin/env python3
"""Migrate a running Plexus instance to Trefoil through their management APIs.

Plexus and Trefoil expose the same `/v0/management/backup` and
`/v0/management/restore` endpoints, so this works for Plexus on SQLite and on
PostgreSQL alike and never touches either database directly.

What moves (config backup, always):
    providers, model aliases, API keys, user quotas, MCP servers and MCP keys,
    system settings and OAuth credentials.
With --with-history (full .tar.gz backup) the operational tables move too:
    request usage, debug logs, inference errors, provider performance and
    cooldowns, quota state and snapshots, MCP usage, responses/conversations.

Secrets: Plexus decrypts API keys and OAuth tokens on export and Trefoil
re-encrypts them on import, so no encryption keys are needed here. Set
ENCRYPTION_KEY and TREFOIL_CREDENTIAL_KEY on Trefoil as for any deployment.
Backup files contain those secrets in plaintext: treat --save-backup output as
a secret and delete it afterwards.

Restore REPLACES the providers, aliases and keys already in Trefoil, so the
script refuses to run against a populated Trefoil unless --yes is given.

Usage:
  PLEXUS_ADMIN_KEY=... TREFOIL_ADMIN_KEY=... python3 plexus_to_trefoil.py \\
      --plexus-url http://plexus:4000 --trefoil-url http://trefoil:8080 \\
      [--with-history] [--dry-run] [--yes]

  # Plexus on a SQLite file: start Plexus against a copy of the file, then
  # point --plexus-url at it. Or, from a backup you already have:
  python3 plexus_to_trefoil.py --backup-file plexus-backup.json \\
      --trefoil-url http://trefoil:8080

Exit status: 0 on success (and verification), 1 on any failure.
"""

import argparse
import json
import os
import sys
import urllib.error
import urllib.request

CONFIG_SECTIONS = ("providers", "models", "keys", "user_quotas", "mcp_servers", "mcp_keys")


class MigrationError(Exception):
    pass


def request(method, url, admin_key, body=None, content_type=None, timeout=600):
    headers = {"x-admin-key": admin_key} if admin_key else {}
    if content_type:
        headers["content-type"] = content_type
    req = urllib.request.Request(url, data=body, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.status, resp.read()
    except urllib.error.HTTPError as err:
        detail = err.read().decode("utf-8", "replace")[:400]
        raise MigrationError(f"{method} {url} -> HTTP {err.code}: {detail}") from None
    except urllib.error.URLError as err:
        raise MigrationError(f"{method} {url} failed: {err.reason}") from None


def base(url):
    return url.rstrip("/")


def fetch_backup(plexus_url, admin_key, full):
    suffix = "?full=true" if full else ""
    _, body = request("GET", f"{base(plexus_url)}/v0/management/backup{suffix}", admin_key)
    return body


def parse_envelope(raw):
    """Return the config envelope from a config-only JSON backup."""
    try:
        envelope = json.loads(raw)
    except ValueError:
        raise MigrationError("backup is not valid JSON") from None
    if envelope.get("plexus_backup") is not True or "data" not in envelope:
        raise MigrationError("not a Plexus config backup (missing plexus_backup/data)")
    return envelope


def summarize(data):
    counts = {}
    for section in CONFIG_SECTIONS + ("settings",):
        value = data.get(section)
        counts[section] = len(value) if value else 0
    counts["oauth_credentials"] = len(data.get("oauth_credentials") or [])
    return counts


def trefoil_is_populated(trefoil_url, admin_key):
    _, body = request("GET", f"{base(trefoil_url)}/v0/management/backup", admin_key)
    data = parse_envelope(body)["data"]
    return {k: v for k, v in summarize(data).items()
            if k in ("providers", "models", "keys") and v}


def verify(trefoil_url, admin_key, expected):
    """Re-export Trefoil's config and compare section sizes with the source."""
    _, body = request("GET", f"{base(trefoil_url)}/v0/management/backup", admin_key)
    actual = summarize(parse_envelope(body)["data"])
    problems = [
        f"{section}: expected {want}, Trefoil has {actual.get(section, 0)}"
        for section, want in expected.items()
        if actual.get(section, 0) != want
    ]
    return actual, problems


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--plexus-url", help="Base URL of the running Plexus instance")
    ap.add_argument("--plexus-admin-key", default=os.environ.get("PLEXUS_ADMIN_KEY"),
                    help="Plexus admin key (env PLEXUS_ADMIN_KEY)")
    ap.add_argument("--backup-file",
                    help="Use an existing Plexus backup (.json config or .tar.gz full) instead of --plexus-url")
    ap.add_argument("--trefoil-url", required=True, help="Base URL of the Trefoil instance")
    ap.add_argument("--trefoil-admin-key", default=os.environ.get("TREFOIL_ADMIN_KEY"),
                    help="Trefoil admin key (env TREFOIL_ADMIN_KEY)")
    ap.add_argument("--with-history", action="store_true",
                    help="Also migrate usage/log history (full .tar.gz backup; can be large)")
    ap.add_argument("--save-backup", metavar="PATH",
                    help="Write the fetched backup to PATH (contains plaintext secrets, mode 0600)")
    ap.add_argument("--dry-run", action="store_true",
                    help="Fetch and summarize the backup; do not touch Trefoil")
    ap.add_argument("--yes", action="store_true",
                    help="Allow replacing providers/aliases/keys already present in Trefoil")
    args = ap.parse_args(argv)

    if bool(args.plexus_url) == bool(args.backup_file):
        ap.error("give exactly one of --plexus-url or --backup-file")
    if args.plexus_url and not args.plexus_admin_key:
        ap.error("--plexus-admin-key (or PLEXUS_ADMIN_KEY) is required with --plexus-url")
    if not args.dry_run and not args.trefoil_admin_key:
        ap.error("--trefoil-admin-key (or TREFOIL_ADMIN_KEY) is required")

    try:
        if args.backup_file:
            with open(args.backup_file, "rb") as handle:
                raw = handle.read()
            print(f"Read {len(raw)} bytes from {args.backup_file}")
        else:
            print(f"Exporting from Plexus at {base(args.plexus_url)} ...")
            raw = fetch_backup(args.plexus_url, args.plexus_admin_key, args.with_history)
            print(f"Received {len(raw)} bytes")

        is_archive = raw[:2] == b"\x1f\x8b"
        if is_archive and not args.with_history and args.backup_file:
            args.with_history = True  # a full archive was supplied explicitly
        if args.with_history and not is_archive:
            raise MigrationError("--with-history needs a full .tar.gz backup, got JSON")

        if args.save_backup:
            fd = os.open(args.save_backup, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
            with os.fdopen(fd, "wb") as handle:
                handle.write(raw)
            print(f"Saved backup to {args.save_backup} (contains secrets; delete after use)")

        if is_archive:
            expected = None  # counts live inside the archive; verified by the restore result
        else:
            expected = summarize(parse_envelope(raw)["data"])
            print("Backup contents: " + ", ".join(f"{k}={v}" for k, v in expected.items()))

        if args.dry_run:
            print("Dry run: Trefoil was not modified.")
            return 0

        existing = trefoil_is_populated(args.trefoil_url, args.trefoil_admin_key)
        if existing and not args.yes:
            raise MigrationError(
                "Trefoil already has " + ", ".join(f"{v} {k}" for k, v in existing.items())
                + "; restore replaces them. Re-run with --yes to proceed."
            )

        content_type = "application/gzip" if is_archive else "application/json"
        print("Restoring into Trefoil ...")
        _, body = request("POST", f"{base(args.trefoil_url)}/v0/management/restore",
                          args.trefoil_admin_key, raw, content_type)
        result = json.loads(body or b"{}")
        if not result.get("success"):
            raise MigrationError(f"restore did not report success: {result}")
        if "restored" in result:
            print("Restored rows: " + ", ".join(f"{k}={v}" for k, v in sorted(result["restored"].items())))

        if is_archive:
            # The archive's config.json is the envelope's `data`; read it back for comparison.
            import io, tarfile
            with tarfile.open(fileobj=io.BytesIO(raw), mode="r:gz") as tar:
                config = json.load(tar.extractfile("config.json"))
            expected = summarize(config.get("data", config))
        actual, problems = verify(args.trefoil_url, args.trefoil_admin_key, expected)
        print("Trefoil now has: " + ", ".join(f"{k}={v}" for k, v in actual.items()))
        if problems:
            raise MigrationError("verification failed:\n  " + "\n  ".join(problems))
        print("Migration complete and verified.")
        return 0
    except MigrationError as err:
        print(f"error: {err}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
