"""Differential oracle: replay the same requests against Plexus and Trefoil.

Plexus runs on 14200 with mock upstream 19001, Trefoil on 18200 with mock
19002 (see stack.sh). A scenario file is JSON:
  {"setup": [[method, path, body], ...],          # admin requests, {{MOCK}} → mock base URL
   "scenarios": [{"name", "method"?, "path", "body"?, "multipart"?, "headers"?, "admin"?, "reset_cooldowns"?}, ...],
   "reset_cooldowns"?: true}                       # clear both gateways' cooldowns before every scenario
For each scenario both gateways get the request (inference key `sk-k`, or the
admin key when "admin": true); the client responses and the upstream requests
each gateway made are normalized (volatile ids/timestamps masked) and diffed.

ORACLE_MASK_HEADERS=1 also masks upstream request headers (volatile keys, random ids).

Usage: python3 oracle.py FILE [--setup] [--only SUBSTR] [--verbose]"""
import json, os, re, sys, urllib.request, urllib.error

# PLEXUS_URL / TREFOIL_URL / PLEXUS_MOCK / TREFOIL_MOCK override the default pair.
GATEWAYS = {"plexus": (os.environ.get("PLEXUS_URL", "http://127.0.0.1:14200"), int(os.environ.get("PLEXUS_MOCK", 19001))),
            "trefoil": (os.environ.get("TREFOIL_URL", "http://127.0.0.1:18200"), int(os.environ.get("TREFOIL_MOCK", 19002)))}
VOLATILE = {"id", "created", "created_at", "system_fingerprint", "request_id", "requestId", "date", "completed_at", "createdAt", "startTime", "expiresAt", "durationMs", "responseId"}
# ORACLE_EXTRA_VOLATILE=a,b adds keys to mask (e.g. timing aggregates for observability reads).
VOLATILE |= {k for k in os.environ.get("ORACLE_EXTRA_VOLATILE", "").split(",") if k}
# ORACLE_MASK_REGEX masks matching substrings inside strings (e.g. `cch=[0-9a-f]{5}`).
MASK_REGEX = re.compile(os.environ["ORACLE_MASK_REGEX"]) if os.environ.get("ORACLE_MASK_REGEX") else None
RANDOM_ID = re.compile(r"^(chatcmpl|msg|resp|call|toolu|fc|rs|gen|req|stream)[-_][A-Za-z0-9_-]{6,}$|^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")

def http(method, url, body=None, headers=None):
    h = dict(headers or {})
    data = None
    if isinstance(body, bytes):
        data = body
    elif body is not None:
        data = body.encode() if isinstance(body, str) else json.dumps(body).encode()
        h.setdefault("content-type", "application/json")
    req = urllib.request.Request(url, method=method, data=data, headers=h)
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return r.status, {k.lower(): v for k, v in r.headers.items()}, r.read().decode(errors="replace")
    except urllib.error.HTTPError as e:
        return e.code, {k.lower(): v for k, v in e.headers.items()}, e.read().decode(errors="replace")

def multipart_body(parts):
    """`"multipart": [{"name", "value"} | {"name", "filename", "content_type"?, "data"}]` → (bytes, content type)."""
    boundary = "oracleboundary7d1a"
    out = b""
    for p in parts:
        disp = f'form-data; name="{p["name"]}"' + (f'; filename="{p["filename"]}"' if "filename" in p else "")
        out += f"--{boundary}\r\nContent-Disposition: {disp}\r\n".encode()
        if "content_type" in p:
            out += f"Content-Type: {p['content_type']}\r\n".encode()
        out += b"\r\n" + (p["data"] if "filename" in p else p["value"]).encode("latin1") + b"\r\n"
    out += f"--{boundary}--\r\n".encode()
    return out, f"multipart/form-data; boundary={boundary}"

def mask(value, key=None):
    if isinstance(value, dict):
        return {k: mask(v, k) for k, v in value.items()}
    if isinstance(value, list):
        return [mask(v) for v in value]
    if key in VOLATILE and value is not None:
        return f"<{key}>"
    if isinstance(value, str) and RANDOM_ID.match(value) and not value.endswith("mock"):
        return "<rid>"
    if isinstance(value, str) and MASK_REGEX:
        return MASK_REGEX.sub("<masked>", value)
    return value

def parse_body(text, content_type):
    if "text/event-stream" in (content_type or ""):
        events = []
        for block in text.split("\n\n"):
            if not block.strip():
                continue
            ev = {}
            for line in block.split("\n"):
                if line.startswith("event:"):
                    ev["event"] = line[6:].strip()
                elif line.startswith("data:"):
                    data = line[5:].strip()
                    try:
                        ev["data"] = mask(json.loads(data))
                    except Exception:
                        ev["data"] = data
                elif line.startswith(":"):
                    ev.setdefault("comments", []).append(line)
            events.append(ev)
        return {"sse": events}
    try:
        return mask(json.loads(text))
    except Exception:
        return text

RESET_COOLDOWNS = False

def run(scenario):
    out = {}
    for name, (base, mock_port) in GATEWAYS.items():
        if RESET_COOLDOWNS or scenario.get("reset_cooldowns"):
            http("DELETE", base + "/v0/management/cooldowns", None, {"x-admin-key": "admin"})
        http("GET", f"http://127.0.0.1:{mock_port}/__log")
        headers = {"x-admin-key": "admin"} if scenario.get("admin") else {"authorization": "Bearer sk-k"}
        headers.update(scenario.get("headers", {}))
        body = scenario.get("body")
        if "multipart" in scenario:
            body, headers["content-type"] = multipart_body(scenario["multipart"])
        status, rh, text = http(scenario.get("method", "POST"), base + scenario["path"], body, headers)
        _, _, log = http("GET", f"http://127.0.0.1:{mock_port}/__log")
        upstream = json.loads(log)
        for u in upstream:
            u["headers"] = {k: ("<auth>" if k in ("authorization", "x-api-key", "x-goog-api-key") and "sk-up" in v else v) for k, v in u["headers"].items()}
            if os.environ.get("ORACLE_MASK_HEADERS"):
                u["headers"] = mask(u["headers"])
            u["body"] = mask(u["body"])
        out[name] = {"status": status, "content_type": (rh.get("content-type") or "").split(";")[0], "body": parse_body(text, rh.get("content-type")), "upstream": upstream}
        if scenario.get("headers_of_interest"):
            out[name]["headers"] = {k: rh.get(k) for k in scenario["headers_of_interest"]}
    return out

def diff(a, b, path=""):
    if type(a) != type(b):
        return [f"{path}: plexus={json.dumps(a)[:300]} trefoil={json.dumps(b)[:300]}"]
    lines = []
    if isinstance(a, dict):
        for k in sorted(set(a) | set(b)):
            if k not in a: lines.append(f"{path}.{k}: only trefoil = {json.dumps(b[k])[:300]}")
            elif k not in b: lines.append(f"{path}.{k}: only plexus = {json.dumps(a[k])[:300]}")
            else: lines += diff(a[k], b[k], f"{path}.{k}")
    elif isinstance(a, list):
        if len(a) != len(b): lines.append(f"{path}: len plexus={len(a)} trefoil={len(b)}")
        for i, (x, y) in enumerate(zip(a, b)): lines += diff(x, y, f"{path}[{i}]")
    elif a != b:
        lines.append(f"{path}: plexus={json.dumps(a)[:300]} trefoil={json.dumps(b)[:300]}")
    return lines

def setup(steps):
    for name, (base, mock_port) in GATEWAYS.items():
        for method, path, body in steps:
            body = json.loads(json.dumps(body).replace("{{MOCK}}", f"http://127.0.0.1:{mock_port}"))
            status, _, text = http(method, base + path, body, {"x-admin-key": "admin"})
            if status >= 300:
                print(f"setup {name} {method} {path} -> {status} {text[:200]}")

def main():
    global RESET_COOLDOWNS
    spec = json.load(open(sys.argv[1]))
    RESET_COOLDOWNS = bool(spec.get("reset_cooldowns"))
    if "--setup" in sys.argv:
        setup(spec.get("setup", []))
    only = sys.argv[sys.argv.index("--only") + 1] if "--only" in sys.argv else None
    verbose = "--verbose" in sys.argv
    total = bad = 0
    for scenario in spec.get("scenarios", []):
        if only and only not in scenario["name"]:
            continue
        total += 1
        r = run(scenario)
        d = diff(r["plexus"], r["trefoil"])
        if d:
            bad += 1
            print(f"DIFF {scenario['name']}")
            for line in d[:40]: print("   ", line)
            if verbose: print(json.dumps(r, indent=1)[:8000])
        else:
            print(f"SAME {scenario['name']}")
    print(f"{bad}/{total} scenarios differ")
    sys.exit(1 if bad else 0)

main()
