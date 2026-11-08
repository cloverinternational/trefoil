# Deployment units

Trefoil is usually deployed with the NixOS module (`services.trefoil`). These plain
systemd units exist for hosts that are not NixOS.

| Unit | Purpose |
|---|---|
| `systemd/trefoil-postgres.service` | PostgreSQL on `127.0.0.1:5432` |
| `systemd/trefoil.service` | the gateway on `127.0.0.1:8080` |

Both bind loopback. Reach the admin UI and `/v0/management` through a reverse proxy
or a private network; nothing here publishes the service.

## Install, then add credentials

The service is meant to be installed **before** its secrets exist:

```sh
sudo install -Dm644 deploy/systemd/trefoil.service deploy/systemd/trefoil-postgres.service /etc/systemd/system/
sudo install -Dm755 deploy/trefoil-start /usr/local/bin/trefoil-start
sudo systemctl daemon-reload
sudo systemctl enable trefoil-postgres trefoil
```

Substitute the `@…@` placeholders first (`@TREFOIL_BIN@`, `@PG_BIN@`, `@STATE_DIR@`,
`@RUN_USER@`, `@ENV_FILE@`, `@DB_URL_FILE@`, `@START_SCRIPT@`).

Then create the two credential files the unit loads and start it:

```sh
sudo install -Dm600 deploy/env.example /etc/trefoil/env              # ADMIN_KEY, TREFOIL_CREDENTIAL_KEY, ENCRYPTION_KEY
sudo install -Dm600 /dev/null /etc/trefoil/database-url              # postgres://…
sudo systemctl restart trefoil
```

Until both files exist the unit fails to start and `Restart=on-failure` retries, so it
comes up by itself once they are in place. Empty values fail immediately with
`trefoil: required secret <name> is empty`.

## Why credentials are files

The unit reads them through systemd `LoadCredential`, so the service user never needs
access to the sources and no secret appears in the unit, in the process environment of
the manager, or in a world-readable store. Values are supplied by whatever the operator
already uses — a secret manager, a `0600` file, a provisioning step.
