{
  description = "Hellas L1 Consensus Node";
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
    rust-overlay,
  }:
    flake-utils.lib.eachDefaultSystem (system: let
      overlays = [(import rust-overlay)];
      pkgs = import nixpkgs {
        inherit system overlays;
      };

      rust-toolchain = pkgs.buildPackages.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      rustPlatform = pkgs.makeRustPlatform {
        rustc = rust-toolchain;
        cargo = rust-toolchain;
      };

      commonArgs = {
        pname = "hellas-node";
        version = "0.1.0";
        src = ./.;
        cargoLock = {
          lockFile = ./Cargo.lock;
        };
        auditable = false;
        defaultFeatures = false;
        buildInputs = with pkgs; [openssl];
        nativeBuildInputs = with pkgs; [pkg-config protobuf];
        checkInputs = with pkgs; [cargo-deny cargo-outdated];
        separateDebugInfo = true;
        meta.mainProgram = "node";
      };

      node = rustPlatform.buildRustPackage commonArgs;
      cli = rustPlatform.buildRustPackage (commonArgs
        // {
          pname = "hellas-cli";
          buildFeatures = ["cli"];
          meta.mainProgram = "hellas";
        });
    in {
      packages = {
        default = node;
        inherit node cli;
      };

      overlays.default = final: _prev: {
        hellas-node = self.packages.${final.system}.node;
        hellas-cli = self.packages.${final.system}.cli;
      };

      devShells.default = pkgs.mkShell {
        inputsFrom = [self.packages.${system}.default];
        buildInputs = with pkgs; [
          pre-commit
          protobuf-language-server
          cargo-watch
          gh
          grpcurl
        ];
      };

      checks = pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
        integration = import ./tests/integration.nix {inherit self pkgs;};
      };
    })
    // {
      nixosModules.hellas = {
        config,
        lib,
        pkgs,
        ...
      }: let
        inherit (lib) mkEnableOption mkIf mkOption types mapAttrs' nameValuePair filterAttrs mapAttrsToList flatten;
        tomlFormat = pkgs.formats.toml {};
        cfg = config.services.hellas;
        enabledValidators = filterAttrs (_: v: v.enable) cfg.validators;
      in {
        options.services.hellas = {
          package = mkOption {
            type = types.package;
            default = self.packages.${pkgs.stdenv.hostPlatform.system}.node;
            description = "Package providing the node binary.";
          };

          cliPackage = mkOption {
            type = types.package;
            default = self.packages.${pkgs.stdenv.hostPlatform.system}.cli;
            description = "Package providing the hellas CLI binary.";
          };

          installCli = mkOption {
            type = types.bool;
            default = true;
            description = "Whether to install the hellas CLI tool into system packages.";
          };

          validators = mkOption {
            type = types.attrsOf (types.submodule {
              options = {
                enable = mkEnableOption "Hellas validator instance";

                openFirewall = mkOption {
                  type = types.bool;
                  default = false;
                  description = "Open firewall ports for this validator instance.";
                };

                p2pPort = mkOption {
                  type = types.port;
                  default = 9000;
                  description = "P2P listen port (used for firewall rules).";
                };

                grpcPort = mkOption {
                  type = types.port;
                  default = 50051;
                  description = "gRPC listen port (used for firewall rules).";
                };

                settings = mkOption {
                  type = tomlFormat.type;
                  default = {};
                  description = "Node configuration as a Nix attrset, converted to TOML.";
                };
              };
            });
            default = {};
            description = "Set of Hellas validator instances to run.";
          };
        };

        config = mkIf (enabledValidators != {}) {
          environment.systemPackages = mkIf cfg.installCli [cfg.cliPackage];

          systemd.services =
            mapAttrs' (
              name: vcfg:
                nameValuePair "hellas-validator-${name}" {
                  description = "Hellas validator node (${name})";
                  wantedBy = ["multi-user.target"];
                  after = ["network-online.target"];
                  wants = ["network-online.target"];
                  environment = {
                    HOME = "/var/lib/hellas-validator-${name}";
                  };
                  serviceConfig = {
                    ExecStart = "${cfg.package}/bin/node run --config ${tomlFormat.generate "hellas-validator-${name}.toml" vcfg.settings}";
                    Restart = "on-failure";
                    DynamicUser = true;
                    StateDirectory = "hellas-validator-${name}";
                    WorkingDirectory = "/var/lib/hellas-validator-${name}";
                  };
                }
            )
            enabledValidators;

          networking.firewall = let
            fwInstances = filterAttrs (_: v: v.openFirewall) enabledValidators;
            tcpPorts = flatten (mapAttrsToList (_: v: [v.p2pPort v.grpcPort]) fwInstances);
          in
            mkIf (fwInstances != {}) {
              allowedTCPPorts = tcpPorts;
            };
        };
      };

      nixosModules.default = self.nixosModules.hellas;
    };
}
