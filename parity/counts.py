#!/usr/bin/env python3
"""Emit deterministic, source-traceable parity inventory counts.

The command is read-only: it never modifies source or evidence files.  The
reference checkout is an explicit input so a missing or malformed checkout is
an error, not a zero count.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import re
import subprocess
import sys


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def required(path: pathlib.Path) -> pathlib.Path:
    if not path.is_file():
        raise RuntimeError(f"required evidence file is missing: {path}")
    return path


def git_revision(root: pathlib.Path) -> str:
    result = subprocess.run(
        ["git", "-C", str(root), "rev-parse", "HEAD"],
        check=True,
        capture_output=True,
        text=True,
    )
    return result.stdout.strip()


def differential_case_count(source: str) -> int:
    """Count case objects, excluding nested request fixtures."""
    total = 0
    for match in re.finditer(r"const\s+(?:CASES|\w+_CASES)\b[^=]*=\s*\[", source):
        start = match.end()
        end = source.find("\n];", start)
        if end < 0:
            raise RuntimeError("unterminated differential case array")
        depth = 0
        quote = None
        escaped = False
        for char in source[start:end]:
            if quote is not None:
                if escaped:
                    escaped = False
                elif char == "\\":
                    escaped = True
                elif char == quote:
                    quote = None
                continue
            if char in "'\"`":
                quote = char
            elif char == "{" and depth == 0:
                total += 1
                depth += 1
            elif char == "{":
                depth += 1
            elif char == "}":
                depth -= 1
    return total


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=pathlib.Path, default=pathlib.Path(__file__).resolve().parent.parent)
    parser.add_argument("--reference-dir", type=pathlib.Path, default=None)
    args = parser.parse_args()
    root = args.root.resolve()
    reference = (args.reference_dir or root / ".reference" / "plexus").resolve()

    try:
        rust_files = sorted((root / "backend" / "src").glob("*.rs"))
        if not rust_files:
            raise RuntimeError("no Rust source files found")
        rust_text = "\n".join(path.read_text() for path in rust_files)
        openapi_path = required(reference / "packages/backend/src/assets/openapi.json")
        openapi = json.loads(openapi_path.read_text())
        if not isinstance(openapi.get("paths"), dict):
            raise RuntimeError(f"OpenAPI paths is not an object: {openapi_path}")
        differential = required(root / "parity" / "differential.ts").read_text()
        counts = {
            "differential_named_cases": differential_case_count(differential),
            "trefoil_router_declarations": rust_text.count(".route("),
            "plexus_openapi_paths": len(openapi["paths"]),
            "plexus_openapi_operations": sum(
                sum(method in {"get", "post", "put", "patch", "delete", "options", "head", "trace"} for method in item)
                for item in openapi["paths"].values()
            ),
            "trefoil_migrations": len(list((root / "backend" / "migrations").glob("*.sql"))),
        }
        evidence = {
            "trefoil_revision": git_revision(root),
            "reference_revision": git_revision(reference),
            "inputs": {
                "differential": {"path": str(root / "parity/differential.ts"), "sha256": sha256(root / "parity/differential.ts")},
                "openapi": {"path": str(openapi_path), "sha256": sha256(openapi_path)},
                "rust_source_glob": "backend/src/*.rs",
                "migrations_glob": "backend/migrations/*.sql",
            },
            "counts": counts,
        }
        print(json.dumps(evidence, indent=2, sort_keys=True))
        return 0
    except (OSError, ValueError, subprocess.CalledProcessError, RuntimeError) as error:
        print(f"parity counts: error: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
