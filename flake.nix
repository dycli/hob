# home: the package, a NixOS module that runs it (services.hob), and checks
# (tests, clippy with warnings denied, the module evaluating).
{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      each = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      source = nixpkgs.lib.fileset.toSource {
        root = ./.;
        fileset = nixpkgs.lib.fileset.unions [
          ./Cargo.toml
          ./Cargo.lock
          ./src
        ];
      };
    in
    {
      packages = each (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "hob";
          version = (nixpkgs.lib.importTOML ./Cargo.toml).package.version;
          src = source;
          cargoLock.lockFile = ./Cargo.lock;
          meta = {
            description = "A home backend: cameras, photos, weather, air and calendar behind one HTTP API and MCP";
            license = with nixpkgs.lib.licenses; [
              mit
              asl20
            ];
            mainProgram = "hob";
          };
        };
      });

      nixosModules.default = import ./nix/module.nix self;

      checks = each (pkgs: {
        package = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        clippy = self.packages.${pkgs.stdenv.hostPlatform.system}.default.overrideAttrs (old: {
          pname = "hob-clippy";
          nativeBuildInputs = old.nativeBuildInputs ++ [ pkgs.clippy ];
          buildPhase = "cargo clippy --release --all-targets --offline -- -D warnings";
          doCheck = false;
          installPhase = "touch $out";
        });
        module =
          (nixpkgs.lib.nixosSystem {
            inherit (pkgs.stdenv.hostPlatform) system;
            modules = [
              self.nixosModules.default
              {
                fileSystems."/" = {
                  device = "none";
                  fsType = "tmpfs";
                };
                services.hob = {
                  enable = true;
                  timeZone = "Europe/London";
                  photos = {
                    immichUrl = "http://127.0.0.1:2283";
                    apiKeyFile = "/run/keys/immich";
                    album = "Wall";
                  };
                  weather = {
                    latitude = 51.48;
                    longitude = -0.01;
                  };
                  air.address = "192.0.2.10";
                  cameras = {
                    list = [
                      {
                        id = "porch";
                        name = "Porch";
                        turns = true;
                      }
                    ];
                    go2rtcUrl = "http://127.0.0.1:1984";
                    frigateUrl = "http://127.0.0.1:5000";
                  };
                };
                boot.loader.grub.enable = false;
                system.stateVersion = "26.05";
              }
            ];
          }).config.system.build.toplevel;
      });

      formatter = each (pkgs: pkgs.nixfmt);
    };
}
