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
2. **Hoja's `validate_model_base_url` rejects loopback and private targets.**
   Pointing Hoja at `http://127.0.0.1:8080/v1` will fail with
   `Model base URL must not point to a local or private address.` Publish trefoil
   behind a TLS-terminating proxy at a public name (`https://trefoil.example/v1`)
   or accept the workspace-wide `HOJA_ALLOW_INSECURE_MODEL_URL=1` downgrade.
