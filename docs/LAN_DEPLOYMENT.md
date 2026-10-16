# Trefoil on a trusted LAN

This is the simplest supported deployment: Docker Compose, private PostgreSQL,
and a same-origin frontend/backend entry point. It uses **HTTP** and is only
appropriate for a trusted LAN. Do not expose this port to the internet or an
untrusted Wi-Fi network; admin keys, API keys, and OAuth callbacks are
credentials. Add HTTPS/Caddy before doing that.

## First run

```sh
cp .env.example .env
sed -i "s/^ADMIN_KEY=.*/ADMIN_KEY=$(openssl rand -hex 32)/" .env
sed -i "s/^TREFOIL_CREDENTIAL_KEY=.*/TREFOIL_CREDENTIAL_KEY=$(openssl rand -hex 32)/" .env
sed -i "s/^POSTGRES_PASSWORD=.*/POSTGRES_PASSWORD=$(openssl rand -hex 24)/" .env
chmod 600 .env
docker compose up -d --build
docker compose ps
curl http://127.0.0.1:${TREFOIL_PORT:-8080}/healthz
```

Open `http://<LAN-IP>:${TREFOIL_PORT:-8080}/` and log in with `ADMIN_KEY`.
PostgreSQL is not published on the host; only the frontend port is exposed.

## Configure providers and OAuth

Use the web UI to create API-key providers or start an OAuth session. For a
browser callback, set `TREFOIL_PUBLIC_URL` in `.env` to the exact browser-visible
origin, then recreate the backend. The frontend proxy forwards the callback path
unchanged to the backend. Device/manual-code flows are preferable for headless
LAN clients. OAuth credentials require the exact
64-hex-character `TREFOIL_CREDENTIAL_KEY`; losing it makes encrypted OAuth data
unrecoverable.

## Operations

```sh
docker compose logs -f backend
docker compose restart backend
docker compose pull   # only when using published images
docker compose up -d --build
docker compose down   # preserves the named PostgreSQL volume
```

Before upgrades, create a backup from the UI or API and copy it off the host.
The named volume `trefoil-postgres` contains the live database. To roll back,
stop the stack, restore the previous image/source, and start it against the
same volume; do not delete the volume during a rollback.

## Security boundaries

- Keep `.env` mode `600`; never commit it.
- Use a strong unique `ADMIN_KEY`, database password, and credential key.
- Do not publish port 5432.
- Restrict the frontend port in your LAN firewall if needed, e.g. to a trusted
  subnet or one management workstation.
- HTTP provides no transport confidentiality. Use a Caddy HTTPS profile before
  crossing network trust boundaries.
