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
      };
    }
    // flake-utils.lib.eachDefaultSystem (system: let
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
        fragpipe-mcp = rawFragpipeMcpPackage;

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
