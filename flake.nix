{
  description = "Background for the COSMIC desktop environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    nix-filter.url = "github:numtide/nix-filter";
    crane = {
      url = "github:ipetkov/crane";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, nix-filter, crane, fenix }:
    flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" ] (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        craneLib = crane.lib.${system}.overrideToolchain fenix.packages.${system}.stable.toolchain;
        pkgDef = {
          src = nix-filter.lib.filter {
            root = ./.;
            include = [
              ./src
              ./cosmic-bg-config
              ./Cargo.toml
              ./Cargo.lock
              ./i18n
              ./i18n.toml
              ./meson.build
              ./meson_options.txt
              ./build-aux
              ./data
              ./po
            ];
          };
          nativeBuildInputs = with pkgs; [
            meson
            pkg-config
          ];
          buildInputs = with pkgs; [
            wayland
            libxkbcommon
            glib
            gtk4
            desktop-file-utils
            ninja # Makes Meson happy
          ];
        };

        cargoArtifacts = craneLib.buildDepsOnly pkgDef;
        cosmic-bg = craneLib.buildPackage (pkgDef // {
          inherit cargoArtifacts;
          configurePhase = "mesonConfigurePhase"; # Enables Meson for setup
        });
      in {
        checks = {
          inherit cosmic-bg;
        };

        packages.default = cosmic-bg;

        apps.default = flake-utils.lib.mkApp {
          drv = cosmic-bg;
        };

        devShells.default = pkgs.mkShell {
          inputsFrom = builtins.attrValues self.checks.${system};
        };
      });

  nixConfig = {
    # Cache for the Rust toolchain in fenix
    extra-substituters = [ "https://nix-community.cachix.org" ];
    extra-trusted-public-keys = [ "nix-community.cachix.org-1:mB9FSh9qf2dCimDSUo8Zy7bkq5CX+/rkCWyvRCYg3Fs=" ];
  };
}
