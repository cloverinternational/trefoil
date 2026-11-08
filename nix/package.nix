{ lib
, rustPlatform
, buildNpmPackage
, importNpmLock
, nodejs_22
, pkg-config
, openssl
, makeWrapper
}:

let
  version = "0.1.0";

  # Admin UI: Vite static build (frontend/package.json -> "build": "tsc -b && vite build").
  frontend = buildNpmPackage {
    pname = "trefoil-frontend";
    inherit version;
    src = ../frontend;
    nodejs = nodejs_22;
    # Resolve straight from frontend/package-lock.json: no npmDepsHash to guess.
    npmDeps = importNpmLock { npmRoot = ../frontend; };
    # buildNpmPackage's default hook validates a npmDepsHash; importNpmLock
    # replaces that validation with the package-lock.json it was given.
    npmConfigHook = importNpmLock.npmConfigHook;
    npmBuildScript = "build";
    installPhase = ''
      runHook preInstall
      mkdir -p $out/share/trefoil
      cp -r dist $out/share/trefoil/ui
      runHook postInstall
    '';
  };
in

rustPlatform.buildRustPackage {
  pname = "trefoil";
  inherit version;
  src = ../.;

  # Cargo.lock carries checksums, so no cargoHash is required.
  cargoLock.lockFile = ../Cargo.lock;
  cargoBuildFlags = [ "--manifest-path" "backend/Cargo.toml" "--locked" ];
  doCheck = false;

  nativeBuildInputs = [ pkg-config makeWrapper ];
  buildInputs = [ openssl ];

  postInstall = ''
    mkdir -p $out/share/trefoil
    cp -r ${frontend}/share/trefoil/ui $out/share/trefoil/ui

    # The module points TREFOIL_UI_DIR here instead of running a separate nginx.
    wrapProgram $out/bin/trefoil-backend \
      --set-default TREFOIL_UI_DIR $out/share/trefoil/ui
  '';

  meta = with lib; {
    description = "Self-hosted LLM API gateway and router";
    homepage = "https://github.com/cloverinternational/trefoil";
    license = licenses.mit;
    mainProgram = "trefoil-backend";
    platforms = platforms.linux;
  };
}
