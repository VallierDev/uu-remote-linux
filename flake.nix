{
  description = "Unofficial Linux host for NetEase UU Remote";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    systems.url = "github:nix-systems/default-linux";
    flake-parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };
  };

  outputs =
    inputs@{ flake-parts, self, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = import inputs.systems;

      imports = [ flake-parts.flakeModules.easyOverlay ];

      flake = {
        nixosModules.default = {
          imports = [ ./packaging/nix/module.nix ];
          nixpkgs.overlays = [ self.overlays.default ];
        };
      };

      perSystem =
        { config, pkgs, ... }:
        {
          packages = {
            uu-remote-linux = pkgs.callPackage ./packaging/nix/package.nix { };
            default = config.packages.uu-remote-linux;
          };

          checks = {
            nixos = pkgs.testers.runNixOSTest (
              import ./packaging/nix/test.nix {
                inherit self;
                inprocAvPackage = config.packages.uu-remote-linux.override {
                  withInprocAv = true;
                  withViewer = false;
                };
              }
            );
          };

          overlayAttrs = { inherit (config.packages) uu-remote-linux; };

          devShells.default = pkgs.mkShell {
            inputsFrom = [ config.packages.default ];
            packages = with pkgs; [
              clippy
              rustfmt
              rust-analyzer
            ];
          };

          formatter = pkgs.nixfmt;
        };
    };
}
