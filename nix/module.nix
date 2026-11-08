{ config, lib, pkgs, ... }:

let
  cfg = config.services.trefoil;
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
        Note: Hoja's `validate_model_base_url` rejects loopback/private targets,
        so a public https name in front of this port is required for Hoja to
        reach trefoil (see HOJA-21).
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

    environmentFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      example = "/run/keys/trefoil.env";
      description = ''
        File holding `ADMIN_KEY`, `TREFOIL_CREDENTIAL_KEY` and optionally
        `ENCRYPTION_KEY`. It is consumed through systemd `LoadCredential`, so it
        must be readable by root and is never copied into the world-readable
        Nix store. `TREFOIL_CREDENTIAL_KEY` must be exactly 64 hex characters.
      '';
    };

    databaseUrlFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      example = "/run/keys/trefoil-database-url";
      description = ''
        File containing the PostgreSQL connection URL
        (`postgres://user:password@host:5432/db`). Consumed via `LoadCredential`.
      '';
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
        assertion = cfg.environmentFile != null;
        message = "services.trefoil.environmentFile must point at a root-owned file containing ADMIN_KEY and TREFOIL_CREDENTIAL_KEY; secrets cannot come from the Nix store.";
      }
      {
        assertion = cfg.databaseUrlFile != null;
        message = "services.trefoil.databaseUrlFile must point at a file containing the PostgreSQL URL.";
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
      serviceConfig.LoadCredential = [
        "env:${toString cfg.environmentFile}"
        "databaseUrl:${toString cfg.databaseUrlFile}"
      ];

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

        # Hardening (HOJA-21 acceptance criterion 3).
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
