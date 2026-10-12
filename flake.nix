{
  description = "Bare-metal multiplayer and Android device test orchestration";

  inputs = {
    harbor.url = "git+https://github.com/caniko/harbor.git?ref=feat/harbor-monorepo-components&rev=e8b1996580e4ecd51dd402edf4a16d9df88e3a1b";

    nixpkgs.follows = "harbor/nixpkgs";
    rust-overlay.follows = "harbor/rust-overlay";
    crane.follows = "harbor/crane";
    flake-utils.url = "github:numtide/flake-utils";

    # The steampipe cluster harness (cluster-ctl + lib.mkTestCluster). fragpipe
    # re-exports it (see `lib.steampipe` and `packages.cluster-ctl` below) so
    # downstream consumers reach steampipe THROUGH fragpipe rather than
    # depending on it directly.
    steampipe = {
      url = "git+https://github.com/caniko/steampipe.git";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.rust-overlay.follows = "rust-overlay";
      inputs.crane.follows = "crane";
      inputs.flake-utils.follows = "flake-utils";
    };
  };

  outputs = {
    self,
    nixpkgs,
    harbor,
    flake-utils,
    rust-overlay,
    steampipe,
    ...
  }: let
    # Wrap `fragpipe-mcp` with the Android SDK/NDK environment variables it
    # needs in order to drive `android-1v1`, `android-ui`, and `android-doctor`.
    # Consumers pass in an `androidComposition` (from `androidenv.composeAndroidPackages`)
    # plus the NDK version and platform they target. The defaults match the
    # values fragpipe assumes when invoked from `nix develop .#android`.
    mkFragpipeMcpAndroidWrapper = {
      pkgs,
      fragpipeMcp,
      androidComposition,
      ndkVersion,
      cargoNdkPlatform ? 28,
      name ? "fragpipe-mcp-android",
      extraRuntimeInputs ? [],
      extraEnv ? {},
    }: let
      ndkRoot = "${androidComposition.androidsdk}/libexec/android-sdk/ndk/${ndkVersion}";
      sdkRoot = "${androidComposition.androidsdk}/libexec/android-sdk";
      extraExports =
        builtins.concatStringsSep "\n"
        (builtins.map (k: ''export ${k}="${builtins.getAttr k extraEnv}"'')
          (builtins.attrNames extraEnv));
    in
      pkgs.writeShellApplication {
        inherit name;
        runtimeInputs =
          [
            androidComposition.androidsdk
            pkgs.cargo-ndk
            pkgs.gradle
            pkgs.jdk21
          ]
          ++ extraRuntimeInputs;
        text = ''
          export ANDROID_NDK_HOME="${ndkRoot}"
          export ANDROID_NDK_ROOT="$ANDROID_NDK_HOME"
          export ANDROID_SDK_ROOT="${sdkRoot}"
          export ANDROID_HOME="$ANDROID_SDK_ROOT"
          export ANDROID_AVD_HOME="''${ANDROID_AVD_HOME:-$HOME/.config/.android/avd}"
          export CARGO_NDK_PLATFORM=${toString cargoNdkPlatform}
          ${extraExports}
          exec ${fragpipeMcp}/bin/fragpipe-mcp "$@"
        '';
      };
  in
    {
      lib = {
        inherit mkFragpipeMcpAndroidWrapper;
        # Re-export the steampipe flake so consumers reach the cluster harness
        # (lib.mkTestCluster, nixosModules, packages) through fragpipe.
        inherit steampipe;
      };
    }
    // flake-utils.lib.eachDefaultSystem (system: let
      pkgs =
        import (
          if system == "x86_64-darwin"
          then harbor.inputs.nixpkgs-darwin
          else nixpkgs
        ) {
          inherit system;
          overlays = [(import rust-overlay)];
        };

      toolchain = harbor.lib.rust.mkToolchain {
        inherit pkgs;
        toolchainProfile = "nightly";
      };
      inherit (toolchain) craneLib;
      treefmtEval = harbor.inputs.treefmt-nix.lib.evalModule pkgs {
        imports = [harbor.treefmtModules.core-nix harbor.treefmtModules.rust-rust];
        projectRootFile = "flake.nix";
        programs.rustfmt.package = toolchain.rustToolchain;
      };
      opencodeLspShell = harbor.inputs.nix-opencode-lsp.lib.mkShell {
        inherit pkgs;
        rustAnalyzer = toolchain.rustToolchain;
      };

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
      rawFragpipeMcpPackage = craneLib.buildPackage (commonArgs
        // {
          inherit cargoArtifacts;
          pname = "fragpipe-mcp";
          cargoExtraArgs = "--bin fragpipe-mcp";
        });
      fragpipeMcpPackage = pkgs.writeShellApplication {
        name = "fragpipe-mcp";
        runtimeInputs = [fragpipePackage];
        text = ''
          exec ${rawFragpipeMcpPackage}/bin/fragpipe-mcp "$@"
        '';
      };
    in {
      packages =
        {
          default = fragpipePackage;
          fragpipe = fragpipePackage;
          fragpipe-mcp = fragpipeMcpPackage;
        }
        // nixpkgs.lib.optionalAttrs (system == "x86_64-linux") {
          # Steampipe supports x86_64 Linux, where consumers pull it via fragpipe.
          cluster-ctl = steampipe.packages.${system}.default;
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
        fragpipe-mcp = rawFragpipeMcpPackage;
        formatting = treefmtEval.config.build.check self;

        clippy = craneLib.cargoClippy (commonArgs
          // {
            inherit cargoArtifacts;
            cargoClippyExtraArgs = "--all-targets -- --deny warnings";
          });

        fmt = craneLib.cargoFmt {
          inherit src;
        };
        # Fail if flake inputs ever point at the retired Codeberg/Codefloe
        # mirrors again (fleet migrated to github.com/caniko/*).
        # sourceUrl package metadata is excluded: informational only, not fetched.
        host-pinning = let
          # Split across literals so this file never matches its own pattern.
          staleHosts = "cod" + "eberg|cod" + "efloe";
        in
          pkgs.runCommand "fragpipe-host-pinning" {} ''
            if ${pkgs.lib.getExe pkgs.ripgrep} -v "sourceUrl" ${./flake.nix} ${./flake.lock} \
              | ${pkgs.lib.getExe pkgs.ripgrep} -q "${staleHosts}"; then
              echo "ERROR: retired forge host in flake inputs:" >&2
              ${pkgs.lib.getExe pkgs.ripgrep} -v "sourceUrl" ${./flake.nix} ${./flake.lock} \
                | ${pkgs.lib.getExe pkgs.ripgrep} -n "${staleHosts}" >&2 || true
              exit 1
            fi
            touch $out
          '';
      };

      formatter = treefmtEval.config.build.wrapper;
      devShells.default = craneLib.devShell {
        checks = self.checks.${system};
        inputsFrom = [opencodeLspShell];
        packages = with pkgs; [
          cargo-nextest
          rust-analyzer
          treefmtEval.config.build.wrapper
        ];
      };
    });
}
