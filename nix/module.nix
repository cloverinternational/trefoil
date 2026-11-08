{ config, lib, pkgs, ... }:

let
  cfg = config.services.trefoil;

  # What the service needs, by name. `environmentFile` and `databaseUrlFile` predate
  # this map and remain as shorthands; `credentials` wins when both are set.
  credentialFiles =
    lib.optionalAttrs (cfg.environmentFile != null) { env = { file = cfg.environmentFile; }; }
    // lib.optionalAttrs (cfg.databaseUrlFile != null) { databaseUrl = { file = cfg.databaseUrlFile; }; }
    // cfg.credentials;

  requiredCredentials = [ "env" "databaseUrl" ];
  missingCredentials =
    builtins.filter (name: !(builtins.hasAttr name credentialFiles)) requiredCredentials;
in
{
  options.services.trefoil = {
    enable = lib.mkEnableOption "trefoil, a self-hosted LLM API gateway and router";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ./package.nix { };
      defaultText = lib.literalExpression "pkgs.callPackage ./package.nix { }";
      description = "The trefoil package to use.";
    };

    address = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1";
      description = ''
        Interface the backend binds to (`PORT` is configured separately).
        Defaults to loopback. Reach it through a reverse proxy or a private
        network, and terminate TLS in front of it.
      '';
    };

    port = lib.mkOption {
      type = lib.types.port;
      default = 8080;
      description = "Port the trefoil backend listens on.";
    };

    logLevel = lib.mkOption {
      type = lib.types.enum [ "error" "warn" "info" "debug" "verbose" "silly" ];
      default = "info";
      description = "Startup log level (`LOG_LEVEL`).";
    };

    databaseMaxConnections = lib.mkOption {
      type = lib.types.ints.positive;
      default = 16;
      description = "PostgreSQL pool size (`DATABASE_MAX_CONNECTIONS`).";
    };

    credentials = lib.mkOption {
      type = lib.types.attrsOf (lib.types.submodule {
        options.file = lib.mkOption {
          type = lib.types.path;
          description = "File staged into the unit under this credential name.";
        };
      });
      default = { };
      example = lib.literalExpression ''
        {
          env.file = "/run/secrets/trefoil-env";
          databaseUrl.file = "/run/secrets/trefoil-database-url";
        }
      '';
      description = ''
        Credentials the service needs, staged by systemd and exposed under
        `$CREDENTIALS_DIRECTORY`. Two names are required:

        - `env` — dotenv with `ADMIN_KEY`, `TREFOIL_CREDENTIAL_KEY` (exactly 64 hex
          characters) and optionally `ENCRYPTION_KEY`;
        - `databaseUrl` — the PostgreSQL connection URL
          (`postgres://user:password@host:5432/db`).

        Each value is only a *path*. Anything that materialises secrets as files
        works — sops-nix, agenix, `systemd.credentials`, a cloud agent — and the
        module does not need to know which. Inline secret values are deliberately
        not accepted: the Nix store is world-readable, and a path keeps the module
        fail-closed without prescribing a specific secret manager.
      '';
    };

    environmentFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      example = "/run/keys/trefoil.env";
      description = ''
        Shorthand for `credentials.env.file`. The file holds `ADMIN_KEY`,
        `TREFOIL_CREDENTIAL_KEY` and optionally `ENCRYPTION_KEY`.
      '';
    };

    databaseUrlFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      example = "/run/keys/trefoil-database-url";
      description = "Shorthand for `credentials.databaseUrl.file`.";
    };

    extraEnvironment = lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = { };
      description = "Extra non-secret environment variables. Never put key material here.";
    };

    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Open `port` in the firewall (only meaningful with a public bind address).";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = missingCredentials == [ ];
        message =
          "services.trefoil is missing credential(s): "
          + lib.concatStringsSep ", " missingCredentials
          + ". Point services.trefoil.credentials.<name>.file at a file your secret manager writes "
          + "(required: env, databaseUrl), or use the environmentFile/databaseUrlFile shorthands.";
      }
    ];

    warnings = lib.optional (cfg.address != "127.0.0.1" && !cfg.openFirewall)
      "services.trefoil binds ${cfg.address} without openFirewall; TLS must terminate in front of trefoil because its HTTP layer is plain.";

    # Dedicated, non-root, no-login system user. Mirrors backend/Dockerfile
    # (`useradd --system --uid 10001 --create-home trefoil`).
    users.groups.trefoil = { };
    users.users.trefoil = {
      isSystemUser = true;
      group = "trefoil";
      home = "/var/lib/trefoil";
      createHome = true;
      description = "trefoil LLM gateway";
    };

    networking.firewall.allowedTCPPorts = lib.optionals cfg.openFirewall [ cfg.port ];

    systemd.services.trefoil = {
      description = "trefoil LLM API gateway and router";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [ "network-online.target" "postgresql.service" ];

      environment = {
        PORT = toString cfg.port;
        LOG_LEVEL = cfg.logLevel;
        DATABASE_MAX_CONNECTIONS = toString cfg.databaseMaxConnections;
      } // cfg.extraEnvironment;

      # Credentials are staged by systemd as root into a per-service tmpfs; the
      # unit's own user never needs read access to the source files.
      serviceConfig.LoadCredential =
        lib.mapAttrsToList (name: cred: "${name}:${toString cred.file}") credentialFiles;

      script = ''
        set -eu
        set -a
        . "$CREDENTIALS_DIRECTORY/env"
        set +a
        export DATABASE_URL="$(cat "$CREDENTIALS_DIRECTORY/databaseUrl")"

        for key in ADMIN_KEY TREFOIL_CREDENTIAL_KEY DATABASE_URL; do
          eval "value=\''${''${key}:-}\'"
          if [ -z "$value" ]; then
            echo "trefoil: required secret $key is empty" >&2
            exit 1
          fi
        done

        exec ${lib.getExe cfg.package}
      '';

      serviceConfig = {
        Type = "simple";
        User = "trefoil";
        Group = "trefoil";
        Restart = "on-failure";
        RestartSec = 5;

        StateDirectory = "trefoil";
        WorkingDirectory = "/var/lib/trefoil";
        UMask = "0077";

        # Hardening.
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        ProtectClock = true;
        ProtectHostname = true;
        ProtectProc = "invisible";
        ProcSubset = "pid";
        RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" ];
        RestrictNamespaces = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        SystemCallArchitectures = [ "native" ];
        SystemCallFilter = [ "@system-service" ];
        SystemCallErrorNumber = "EPERM";
        CapabilityBoundingSet = [ "" ];
        AmbientCapabilities = [ "" ];
        DevicePolicy = "closed";
        RemoveIPC = true;
        KeyringMode = "private";
        ReadWritePaths = [ "/var/lib/trefoil" ];
      };
    };
  };
}
