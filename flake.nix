{
  description = "Bare-metal multiplayer and Android device test orchestration";

  inputs = {
    rs-harbor.url = "git+ssh://git@codeberg.org/caniko/rs-harbor.git";

    nixpkgs.follows = "rs-harbor/nixpkgs";
    rust-overlay.follows = "rs-harbor/rust-overlay";
    crane.follows = "rs-harbor/crane";
    flake-utils.follows = "rs-harbor/flake-utils";
  };

  outputs = {
    self,
    nixpkgs,
    rs-harbor,
    flake-utils,
    rust-overlay,
    ...
  }:
    flake-utils.lib.eachDefaultSystem (system: let
      pkgs = import nixpkgs {
        inherit system;
        overlays = [(import rust-overlay)];
      };

      toolchain = rs-harbor.lib.mkToolchain {inherit pkgs;};
      inherit (toolchain) craneLib;

      src = craneLib.cleanCargoSource ./.;

      commonArgs = {
        inherit src;
        strictDeps = true;
      };

      cargoArtifacts = craneLib.buildDepsOnly commonArgs;

      fragpipePackage = craneLib.buildPackage (commonArgs
        // {
          inherit cargoArtifacts;
          cargoExtraArgs = "--bin fragpipe";
        });
      fragpipeMcpPackage = craneLib.buildPackage (commonArgs
        // {
          inherit cargoArtifacts;
          pname = "fragpipe-mcp";
          cargoExtraArgs = "--bin fragpipe-mcp";
        });
    in {
      packages = {
        default = fragpipePackage;
        fragpipe = fragpipePackage;
        fragpipe-mcp = fragpipeMcpPackage;
      };

      apps = {
        default = {
          type = "app";
          program = "${fragpipePackage}/bin/fragpipe";
        };
        fragpipe = {
          type = "app";
          program = "${fragpipePackage}/bin/fragpipe";
        };
        fragpipe-mcp = {
          type = "app";
          program = "${fragpipeMcpPackage}/bin/fragpipe-mcp";
        };
      };

      checks = {
        default = fragpipePackage;
        fragpipe-mcp = fragpipeMcpPackage;

        clippy = craneLib.cargoClippy (commonArgs
          // {
            inherit cargoArtifacts;
            cargoClippyExtraArgs = "--all-targets -- --deny warnings";
          });

        fmt = craneLib.cargoFmt {
          inherit src;
        };
      };

      devShells.default = craneLib.devShell {
        checks = self.checks.${system};
        packages = with pkgs; [
          cargo-nextest
          rust-analyzer
        ];
      };
    });
}
