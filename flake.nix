{
  description = "trefoil — self-hosted LLM API gateway and router";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f:
        nixpkgs.lib.genAttrs systems (system: f (import nixpkgs { inherit system; }));
    in
    {
      packages = forAllSystems (pkgs: {
        default = pkgs.callPackage ./nix/package.nix { };
        trefoil = pkgs.callPackage ./nix/package.nix { };
      });

      nixosModules = {
        trefoil = import ./nix/module.nix;
        default = self.nixosModules.trefoil;
      };

      formatter = forAllSystems (pkgs: pkgs.nixpkgs-fmt);
    };
}
