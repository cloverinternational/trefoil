"""Seed a running Plexus with representative config through its management API.

Usage: seed_plexus.py BASE_URL ADMIN_KEY
Covers encrypted-at-rest secrets, header/extraBody/quirks/quota-checker provider
settings, multi-target aliases, a key bound to a quota, an MCP server with a key,
a system setting, and an OAuth credential (seeded through Plexus's own restore,
the only way to create one without a browser login).
"""
import json
import sys
import urllib.request

base, admin = sys.argv[1].rstrip("/"), sys.argv[2]
headers = {"x-admin-key": admin, "content-type": "application/json"}


def call(method, path, body=None):
    req = urllib.request.Request(
        base + path, method=method, headers=headers,
        data=None if body is None else json.dumps(body).encode(),
    )
    with urllib.request.urlopen(req, timeout=30) as resp:
        return json.load(resp) if resp.length != 0 else {}


call("PUT", "/v0/management/providers/acme", {
    "api_base_url": "https://api.acme.test/v1", "api_key": "sk-acme-SECRET-1",
    "enabled": True, "discount": 0.1, "estimateTokens": True,
    "headers": {"X-Org": "clover", "X-Team": "platform"}, "extraBody": {"top_k": 40},
    "models": {"m1": {"pricing": {"source": "simple", "input": 1, "output": 2}}, "m2": {}},
    "pi_ai_quirks": {"chat": {"api": "openai-completions", "reasoning": True}},
    "quota_checker": {"type": "synthetic", "enabled": True, "intervalMinutes": 15,
                      "options": {"apiKey": "qc-SECRET"}},
})
call("PUT", "/v0/management/providers/beta", {
    "api_base_url": {"chat": "https://beta.test/v1", "messages": "https://beta.test/anthropic"},
    "api_key": "sk-beta-SECRET-2", "models": ["x1", "x2"], "timeout_ms": 45000,
    "max_concurrency": 3,
})
call("PUT", "/v0/management/aliases/fast", {
    "selector": "random", "additional_aliases": ["quick"],
    "targets": [{"provider": "acme", "model": "m1"}, {"provider": "beta", "model": "x1"}],
})
call("PUT", "/v0/management/aliases/smart", {
    "selector": "in_order", "targets": [{"provider": "beta", "model": "x2"}],
    "extraBody": {"temperature": 0.2},
})
call("PUT", "/v0/management/user-quotas/daily-cap",
     {"type": "daily", "limitType": "requests", "limit": 500})
call("PUT", "/v0/management/keys/team-a",
     {"secret": "sk-plexus-team-a-SECRET", "comment": "team a", "quota": "daily-cap"})
call("PUT", "/v0/management/mcp-servers/docs", {
    "upstream_url": "https://mcp.example.test/mcp",
    "headers": {"Authorization": "Bearer mcp-SECRET"},
})
call("POST", "/v0/management/mcp-servers/docs/keys", {"key": "mcp-key-SECRET"})
call("PATCH", "/v0/management/system-settings", {"failover.enabled": True})

envelope = call("GET", "/v0/management/backup")
envelope["data"]["oauth_credentials"] = [{
    "provider_type": "github-copilot", "account_id": "work",
    "access_token": "gho_ACCESS_SECRET", "refresh_token": "ghr_REFRESH_SECRET",
    "enterprise_url": "ghe.example.test", "expires_at": 4102444800000,
}]
call("POST", "/v0/management/restore", envelope)
print("seeded Plexus")
