# Public edge

Trefoil serves two audiences with opposite exposures:

| Surface | Audience | Exposure |
|---|---|---|
| `/v1/*`, `/healthz` | Hoja Cloud tenants (inference) | **public HTTPS** |
| `/v0/management/*`, `/ui/*`, `/assets/*` | operators | **tailnet only** |

`public-edge.Caddyfile` implements that split: it proxies exactly `/v1/*` and
`/healthz` to `127.0.0.1:8080` with buffering disabled for SSE, and answers `404`
for everything else. The admin UI is reached through `tailscale serve`, never
through this edge.

## Prerequisites

1. Trefoil listening on loopback (`TREFOIL_BIND_ADDRESS=127.0.0.1`).
2. A DNS record pointing the edge hostname **directly at this host**, unproxied.
   Behind a CDN proxy, HTTP-01 issuance fails and the CDN terminates TLS instead.
3. Ports 80 (ACME) and 443 reachable from the internet.

## Install

```sh
install -Dm644 deploy/public-edge.Caddyfile /etc/trefoil-edge/Caddyfile
install -Dm644 deploy/trefoil-edge.service /etc/systemd/system/trefoil-edge.service
systemctl daemon-reload
ufw allow 80/tcp && ufw allow 443/tcp
systemctl enable --now trefoil-edge
```

## Verification

```sh
curl -fsS https://<edge-host>/healthz                       # ok
curl -s -o /dev/null -w '%{http_code}\n' https://<edge-host>/v0/management/keys   # 404 from the edge
curl -s -o /dev/null -w '%{http_code}\n' https://<edge-host>/ui/                  # 404 from the edge
curl -N -X POST https://<edge-host>/v1/chat/completions ... # streams (SSE)
```

The third check is the one that matters: `/v0/management/keys` must return the
edge's 404 even when a **valid admin key** is supplied, because the edge never
forwards that path.
