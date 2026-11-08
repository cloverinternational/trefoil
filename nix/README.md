# Deploying trefoil with Nix

Upstream trefoil ships only a Docker Compose path. This directory adds a flake
and a NixOS module so the service can be built and run natively on NixOS.

## What is provided

| Output | Meaning |
|---|---|
| `packages.default` | `trefoil-backend` binary + built admin UI in `share/trefoil/ui` |
| `nixosModules.default` | `services.trefoil.*` |

## Flake outputs

```sh
nix build .#default            # backend + UI
nix eval .#nixosModules.default --apply builtins.attrNames
```

## NixOS usage

Secrets must never enter the Nix store. Put them in root-owned files and let
systemd stage them per-service via `LoadCredential`:

```sh
# /run/keys/trefoil.env   (root:root, 0600)
ADMIN_KEY=<openssl rand -hex 32>
TREFOIL_CREDENTIAL_KEY=<openssl rand -hex 32>   # exactly 64 hex chars
ENCRYPTION_KEY=<openssl rand -hex 32>           # recommended; empty = plaintext

# /run/keys/trefoil-database-url   (root:root, 0600)
postgres://trefoil:<password>@127.0.0.1:5432/trefoil
```

```nix
{
  services.trefoil = {
    enable = true;
    environmentFile = "/run/keys/trefoil.env";
    databaseUrlFile = "/run/keys/trefoil-database-url";
    address = "127.0.0.1";
    port = 8080;
  };
}
```

The service runs as a dedicated `trefoil` system user with `ProtectSystem=strict`,
`NoNewPrivileges`, an empty capability set and `MemoryDenyWriteExecute`.

## Two constraints worth knowing before you deploy

1. **Migrations run at startup** (`sqlx::migrate!()`); PostgreSQL 16 must already
   be reachable via `DATABASE_URL`.
2. **Exposure is a deployment decision, not a trefoil feature.** The module binds
   loopback by default. If clients on other hosts must reach the API, put a
   TLS-terminating reverse proxy - or a private network - in front of it. Some
   client-side HTTP guards refuse loopback and private addresses; the gateway
   cannot relax that for you.
