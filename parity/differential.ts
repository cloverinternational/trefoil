#!/usr/bin/env bun
// Differential parity harness: sends identical requests to a running Plexus
// reference instance and the Rust Trefoil backend, and compares normalized
// responses for the currently ported vertical slice
// (GET /v0/management/auth/verify).
//
// Usage:
//   PLEXUS_BASE_URL=http://127.0.0.1:4100 \
//   TREFOIL_BASE_URL=http://127.0.0.1:8080 \
//   bun run parity/differential.ts
//
// Both servers must already be running. This script sends no real
// credentials or production requests — only the fixed test values below,
// matching packages/backend/src/routes/management/__tests__/admin-auth.test.ts.

const PLEXUS_BASE_URL = process.env.PLEXUS_BASE_URL ?? 'http://127.0.0.1:4100';
const TREFOIL_BASE_URL = process.env.TREFOIL_BASE_URL ?? 'http://127.0.0.1:8080';

// Both servers must be configured with these fixed credentials for the
// comparison to be meaningful:
//   Plexus:  ADMIN_KEY=correct-admin-key, one api key named "test-key" with
//            secret "sk-test-secret"
//   Trefoil: same admin key / key config wired into AppState for this run.
const ADMIN_KEY = 'correct-admin-key';
const LIMITED_KEY_SECRET = 'sk-test-secret';

interface Case {
  name: string;
  header?: string;
}

// Cases mirror admin-auth.test.ts one-for-one.
const CASES: Case[] = [
  { name: 'admin key -> 200 admin principal', header: ADMIN_KEY },
  { name: 'limited key secret -> 200 limited principal', header: LIMITED_KEY_SECRET },
  { name: 'wrong key -> 401', header: 'wrong-key' },
  { name: 'missing header -> 401', header: undefined },
  { name: 'empty header -> 401', header: '' },
];

async function fetchVerify(baseUrl: string, header: string | undefined) {
  const headers: Record<string, string> = {};
  if (header !== undefined) headers['x-admin-key'] = header;
  const res = await fetch(`${baseUrl}/v0/management/auth/verify`, { headers });
  const body = await res.json().catch(() => null);
  return { status: res.status, body };
}

function normalize(body: unknown): unknown {
  // No nondeterministic fields in this contract today (no timestamps/ids),
  // but keep a hook here for future endpoints that need normalization
  // before comparison (e.g. request ids, timestamps).
  return body;
}

// Structural comparison, independent of key insertion order (Fastify/JS
// object literals and Rust serde_json output insertion-order their keys
// differently even when semantically identical).
function deepEqual(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  if (typeof a !== typeof b) return false;
  if (a === null || b === null) return a === b;
  if (typeof a !== 'object') return false;
  if (Array.isArray(a) !== Array.isArray(b)) return false;
  if (Array.isArray(a) && Array.isArray(b)) {
    if (a.length !== b.length) return false;
    return a.every((item, i) => deepEqual(item, b[i]));
  }
  const aObj = a as Record<string, unknown>;
  const bObj = b as Record<string, unknown>;
  const aKeys = Object.keys(aObj).sort();
  const bKeys = Object.keys(bObj).sort();
  if (aKeys.length !== bKeys.length || aKeys.some((k, i) => k !== bKeys[i])) return false;
  return aKeys.every((k) => deepEqual(aObj[k], bObj[k]));
}

async function main() {
  let failures = 0;

  for (const testCase of CASES) {
    const [plexus, trefoil] = await Promise.all([
      fetchVerify(PLEXUS_BASE_URL, testCase.header),
      fetchVerify(TREFOIL_BASE_URL, testCase.header),
    ]);

    const statusMatch = plexus.status === trefoil.status;
    const bodyMatch = deepEqual(normalize(plexus.body), normalize(trefoil.body));
    const pass = statusMatch && bodyMatch;
    if (!pass) failures += 1;

    console.log(`${pass ? 'PASS' : 'FAIL'} ${testCase.name}`);
    if (!pass) {
      console.log(`  plexus:  ${plexus.status} ${JSON.stringify(plexus.body)}`);
      console.log(`  trefoil: ${trefoil.status} ${JSON.stringify(trefoil.body)}`);
    }
  }

  if (failures > 0) {
    console.error(`\n${failures}/${CASES.length} case(s) diverged.`);
    process.exit(1);
  }
  console.log(`\nAll ${CASES.length} cases match.`);
}

main();
