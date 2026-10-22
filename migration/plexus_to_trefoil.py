#!/usr/bin/env python3
"""Migrate a production Plexus SQLite database into a Trefoil PostgreSQL database.

Source: a Plexus SQLite database (drizzle lineage), e.g. a copy of `plexus.db`
        taken while Plexus is stopped. Never commit the database or the output.
Target: Trefoil PostgreSQL (schema produced by backend/migrations/*.sql).

What is migrated (config/state, FK-safe order):
    oauth_credentials -> providers -> provider_models -> model_aliases ->
    model_alias_targets -> api_keys -> user_quota_definitions ->
    mcp_servers -> system_settings -> alias_metadata_overrides

Optional history (--with-history): request_usage, meter_snapshots,
mcp_request_usage, inference_errors, provider_performance, provider_cooldowns,
quota_state, quota_snapshots, debug_logs, mcp_debug_logs.

Credentials:
  - Plexus encrypts provider.api_key and oauth_credentials tokens as
    `enc:v1:<iv>:<tag>:<ct>` (AES-256-GCM, key = ENCRYPTION_KEY hex or
    scrypt(N=16384, salt='plexus-encryption-key-derivation')). Trefoil cannot
    read that envelope, so this script decrypts and re-encrypts:
      * oauth tokens -> Trefoil envelope `v1:<b64(nonce||ct||tag)>` with AAD
        "trefoil/oauth/v1" + length-prefixed provider/account/field
        (key = TREFOIL_CREDENTIAL_KEY, 64 hex chars). TREFOIL_CREDENTIAL_KEY
        must be the same at Trefoil runtime.
      * provider.api_key / api_keys.secret -> plaintext in the generated SQL.
        Start Trefoil with the same ENCRYPTION_KEY and it re-encrypts these
        rows as `enc:v1:` at startup (database::encrypt_plaintext_secrets).
        Treat any --out SQL file as a secret and delete it after applying.

Usage:
  python3 plexus_to_trefoil.py --db plexus-snapshot.db --dsn "$DATABASE_URL" \
      --apply [--with-history]

The script is idempotent: it TRUNCATEs the target tables it manages before
inserting. Source IDs are preserved and sequences re-synced.
"""

import argparse
import base64
import json
import os
import sqlite3
import subprocess
import sys

from cryptography.hazmat.primitives.ciphers.aead import AESGCM
from cryptography.hazmat.primitives.kdf.scrypt import Scrypt

# (source_table, target_table, unique_conflict_keys, encrypted_fields)
CONFIG_TABLES = [
    ("oauth_credentials", "oauth_credentials", None, ("access_token", "refresh_token")),
    ("providers", "providers", ("slug",), ("api_key",)),
    ("provider_models", "provider_models", ("provider_id", "model_name"), ()),
    ("model_aliases", "model_aliases", ("slug",), ()),
    ("model_alias_targets", "model_alias_targets", ("alias_id", "provider_slug", "model_name"), ()),
    ("api_keys", "api_keys", ("name",), ("secret",)),
    ("user_quota_definitions", "user_quota_definitions", ("name",), ()),
    ("mcp_servers", "mcp_servers", ("name",), ()),
    ("system_settings", "system_settings", None, ("value",)),  # value jsonb, not encrypted
    ("alias_metadata_overrides", "alias_metadata_overrides", ("alias_id",), ()),
]

HISTORY_TABLES = [
    ("request_usage", "request_usage", None, ()),
    ("meter_snapshots", "meter_snapshots", None, ()),
    ("mcp_request_usage", "mcp_request_usage", None, ()),
    ("inference_errors", "inference_errors", None, ()),
    ("provider_performance", "provider_performance", None, ()),
    ("provider_cooldowns", "provider_cooldowns", None, ()),
    ("quota_state", "quota_state", None, ()),
    ("quota_snapshots", "quota_snapshots", None, ()),
    ("debug_logs", "debug_logs", None, ()),
    ("mcp_debug_logs", "mcp_debug_logs", None, ()),
]

PLEXUS_PREFIX = "enc:v1:"
PLEXUS_SALT = b"plexus-encryption-key-derivation"
PLEXUS_SCRYPT_N = 16384
TREFOIL_AAD_DOMAIN = b"trefoil/oauth/v1"
PSQL_FIELD_SEP = "\x1f"  # unit separator; never appears in identifiers or enum labels

# Legacy source values that newer Trefoil enums no longer carry.
# 'auto' in Plexus production meant "resolve automatically", which Trefoil
# represents as NULL.
LEGACY_ENUM_MAP = {
    ("model_aliases", "metadata_source"): {"auto": None},
}


class Crypto:
    def __init__(self, plexus_key_raw, trefoil_key_hex):
        self.plexus_key = self._plexus_key(plexus_key_raw) if plexus_key_raw else None
        self.trefoil_key = bytes.fromhex(trefoil_key_hex) if trefoil_key_hex else None

    @staticmethod
    def _plexus_key(raw):
        raw = raw.strip()
        if len(raw) == 64 and all(c in "0123456789abcdefABCDEF" for c in raw):
            return bytes.fromhex(raw)
        kdf = Scrypt(raw.encode(), PLEXUS_SALT, 32, n=PLEXUS_SCRYPT_N, r=8, p=1)
        return kdf.derive()

    def decrypt_plexus(self, value):
        """Decrypt `enc:v1:<iv_hex>:<tag_hex>:<ct_hex>`; plaintext passes through."""
        if value is None or not value.startswith(PLEXUS_PREFIX):
            return value
        if self.plexus_key is None:
            raise SystemExit(
                "Encountered Plexus-encrypted data but no decryption key was provided. "
                "Set --encrypt-key or ENCRYPTION_KEY (the value your Plexus deployment uses)."
            )
        iv_hex, tag_hex, ct_hex = value[len(PLEXUS_PREFIX):].split(":")
        aes = AESGCM(self.plexus_key)
        return aes.decrypt(bytes.fromhex(iv_hex), bytes.fromhex(ct_hex) + bytes.fromhex(tag_hex), None).decode("utf-8")

    def encrypt_trefoil(self, provider, account, field, plaintext):
        if plaintext is None:
            return None
        if self.trefoil_key is None:
            raise SystemExit(
                "Trefoil credential re-encryption requires --credential-key or "
                "TREFOIL_CREDENTIAL_KEY (64 hex chars); use the same key at Trefoil runtime."
            )
        aad = TREFOIL_AAD_DOMAIN
        for component in (provider, account, field):
            encoded = component.encode("utf-8")
            aad += len(encoded).to_bytes(8, "big") + encoded
        nonce = os.urandom(12)
        aes = AESGCM(self.trefoil_key)
        sealed = aes.encrypt(nonce, plaintext.encode("utf-8"), aad)
        return "v1:" + base64.b64encode(nonce + sealed).decode("ascii")


def sql_literal(value):
    if value is None:
        return "NULL"
    if isinstance(value, bool):
        return "TRUE" if value else "FALSE"
    if isinstance(value, (int, float)):
        return repr(value)
    return "'" + str(value).replace("'", "''") + "'"


def load_target_columns(psql_dsn):
    """Return {table: {column: data_type}} for the Trefoil database."""
    query = """
        SELECT table_name, column_name, data_type, udt_name
        FROM information_schema.columns
        WHERE table_schema = 'public'
    """
    enum_query = """
        SELECT n.nspname, t.typname, e.enumlabel
        FROM pg_type t
        JOIN pg_namespace n ON n.oid = t.typnamespace
        JOIN pg_enum e ON e.enumtypid = t.oid
    """
    cols, enums = {}, {}
    for q, sink in ((query, "cols"), (enum_query, "enums")):
        proc = subprocess.run(
            ["psql", psql_dsn, "--no-psqlrc", "-At", "-P", "pager=off",
             "-F", PSQL_FIELD_SEP, "-c", q],
            capture_output=True, text=True, check=True,
            env={**os.environ, "PAGER": "cat"},
        )
        if proc.returncode != 0 or not proc.stdout.strip():
            raise SystemExit(f"Schema introspection query failed: {proc.stderr.strip()}")
        for line in proc.stdout.splitlines():
            parts = line.split("\x1f")
            if sink == "cols" and len(parts) == 4:
                table, column, dtype, udt_name = parts
                cols.setdefault(table, {})[column] = (
                    "enum:" + udt_name if dtype == "USER-DEFINED" else dtype
                )
            elif sink == "enums" and len(parts) == 3:
                enums.setdefault(parts[1], set()).add(parts[2])
    return cols, enums


def coerce(table, column, src_value, target_type, enums):
    if src_value is None:
        return None
    if target_type == "boolean":
        if isinstance(src_value, bool):
            return src_value
        if isinstance(src_value, int):
            return src_value != 0
        return str(src_value).lower() in ("1", "true", "t", "yes")
    if target_type == "jsonb":
        if isinstance(src_value, (dict, list)):
            return json.dumps(src_value)
        text = str(src_value)
        try:
            return json.dumps(json.loads(text))
        except (json.JSONDecodeError, ValueError):
            raise SystemExit(f"Column {table}.{column} is jsonb but source value is not JSON: {text[:80]!r}")
    if target_type.startswith("enum:"):
        label = str(src_value)
        if label in LEGACY_ENUM_MAP.get((table, column), {}):
            return LEGACY_ENUM_MAP[(table, column)][label]
        enum_name = target_type[5:]
        if label not in enums.get(enum_name, set()):
            raise SystemExit(
                f"Column {table}.{column}: value {label!r} is not a label of enum {enum_name}. "
                f"Known labels: {sorted(enums.get(enum_name, set()))}"
            )
        return label
    if target_type in ("integer", "bigint"):
        return int(src_value)
    if target_type in ("real", "double precision", "numeric"):
        return float(src_value)
    return str(src_value)


def migrate_table(sqlite, table, target, conflict_keys, encrypted_fields,
                  crypto, target_cols, enums, out, label):
    if target not in target_cols:
        print(f"  [{label}] skipped: target table {target} does not exist")
        return 0
    src_cols = [r[1] for r in sqlite.execute(f"PRAGMA table_info(`{table}`)")]
    shared = [c for c in src_cols if c in target_cols[target]]
    dropped = [c for c in src_cols if c not in target_cols[target]]
    if dropped:
        print(f"  [{label}] dropping source-only columns: {', '.join(dropped)}")

    count = 0
    for row in sqlite.execute(f"SELECT {', '.join(chr(96)+c+chr(96) for c in shared)} FROM `{table}`"):
        values = dict(zip(shared, row))
        # Plexus encrypts any config TEXT value that may hold a secret
        # (provider api_key/headers/extra_body, quota checker options,
        # alias advanced/extra_body, mcp headers, api key secrets, ...).
        # Trefoil stores these as plaintext, so decrypt universally before
        # any other handling.
        for col, val in values.items():
            if isinstance(val, str) and val.startswith(PLEXUS_PREFIX):
                values[col] = crypto.decrypt_plexus(val)
        # Credential translation.
        provider = values.get("oauth_provider_type") or values.get("provider")
        account = values.get("account_id") or values.get("slug") or values.get("name") or ""
        for field in encrypted_fields:
            if field not in values:
                continue
            plaintext = crypto.decrypt_plexus(values[field])
            if target == "oauth_credentials":
                values[field] = crypto.encrypt_trefoil(str(provider), str(account), field, plaintext)
            else:
                values[field] = plaintext
        # jsonb passthrough for system_settings.value even without key.
        coerced = {
            col: coerce(target, col, val, target_cols[target][col], enums)
            for col, val in values.items()
        }
        cols_sql = ", ".join(f'"{c}"' for c in shared)
        vals_sql = ", ".join(sql_literal(coerced[c]) for c in shared)
        if conflict_keys:
            key_sql = ", ".join(f'"{c}"' for c in conflict_keys)
            updates = ", ".join(f'"{c}" = EXCLUDED."{c}"' for c in shared
                                if c not in conflict_keys and c != "id")
            if updates:
                out.write(f'INSERT INTO "{target}" ({cols_sql}) VALUES ({vals_sql}) '
                          f"ON CONFLICT ({key_sql}) DO UPDATE SET {updates};\n")
            else:  # all columns are the conflict key
                out.write(f'INSERT INTO "{target}" ({cols_sql}) VALUES ({vals_sql}) '
                          f"ON CONFLICT ({key_sql}) DO NOTHING;\n")
        else:
            out.write(f'INSERT INTO "{target}" ({cols_sql}) VALUES ({vals_sql});\n')
        count += 1
    print(f"  [{label}] {count} rows staged")
    return count


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--db", required=True, help="Path to Plexus SQLite database (a snapshot copy)")
    ap.add_argument("--dsn", help="Target PostgreSQL DSN; with --apply the generated SQL runs immediately")
    ap.add_argument("--out", default="migration-out.sql", help="Output SQL file (default: migration-out.sql)")
    ap.add_argument("--apply", action="store_true", help="Apply generated SQL via psql (requires --dsn)")
    ap.add_argument("--with-history", action="store_true", help="Also migrate usage/history tables (large)")
    ap.add_argument("--encrypt-key", default=os.environ.get("ENCRYPTION_KEY") or os.environ.get("PLEXUS_ENCRYPTION_KEY"),
                    help="Plexus ENCRYPTION_KEY (hex or passphrase) for decrypting credentials")
    ap.add_argument("--credential-key", default=os.environ.get("TREFOIL_CREDENTIAL_KEY"),
                    help="Trefoil credential key (64 hex) for re-encrypting OAuth tokens")
    args = ap.parse_args()
    if args.apply and not args.dsn:
        ap.error("--apply requires --dsn")

    crypto = Crypto(args.encrypt_key, args.credential_key)
    sqlite = sqlite3.connect(f"file:{args.db}?mode=ro", uri=True)

    tables = list(CONFIG_TABLES) + (HISTORY_TABLES if args.with_history else [])
    def display_dsn(dsn):
        # Redact the password component before printing.
        if not dsn:
            return "(deferred)"
        head, sep, tail = dsn.partition("://")
        if sep and "@" in tail:
            creds, _, host = tail.rpartition("@")
            user = creds.split(":")[0]
            return f"{head}://{user}:<redacted>@{host}"
        return dsn

    print(f"Inspecting target schema at {display_dsn(args.dsn)} ...")
    target_cols, enums = load_target_columns(args.dsn) if args.dsn else ({}, {})

    # Preview pass (row counts) works without a DSN by retrying schema load after connect.
    with open(args.out, "w", encoding="utf-8") as out:
        out.write("-- Generated by plexus_to_trefoil.py — DO NOT EDIT\n")
        out.write("BEGIN;\n")
        # Truncate in reverse dependency order for a clean, idempotent load.
        for _, target, _, _ in reversed(tables):
            if target in target_cols:
                out.write(f'TRUNCATE TABLE "{target}" CASCADE;\n')
        for src, target, conflict_keys, encrypted in tables:
            migrate_table(sqlite, src, target, conflict_keys, encrypted,
                          crypto, target_cols, enums, out, src)
        # Re-sync serial sequences after explicit-id inserts.
        for _, target, _, _ in tables:
            if target in target_cols and "id" in target_cols[target]:
                out.write(
                    f"SELECT CASE WHEN pg_get_serial_sequence('{target}', 'id') IS NOT NULL THEN "
                    f"setval(pg_get_serial_sequence('{target}', 'id'), "
                    f"COALESCE((SELECT MAX(id) FROM \"{target}\"), 1), "
                    f"COALESCE((SELECT MAX(id) FROM \"{target}\") IS NOT NULL, true)) END;\n"
                )
        out.write("COMMIT;\n")

    print(f"Wrote {args.out}")
    if args.apply:
        proc = subprocess.run(
            ["psql", args.dsn, "--no-psqlrc", "-v", "ON_ERROR_STOP=1", "-f", args.out],
        )
        if proc.returncode != 0:
            raise SystemExit(f"psql failed with exit code {proc.returncode}")
        print("Applied successfully.")


if __name__ == "__main__":
    main()
