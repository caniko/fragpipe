# fragpipe

<!-- simit:badges:start -->
[![CI](https://img.shields.io/badge/CI-managed-2088ff)](.forgejo/workflows/ci.yaml) [![Nix](https://img.shields.io/badge/Nix-managed-5277c3)](flake.nix) [![crates.io](https://img.shields.io/badge/crates.io-ready-f46623)](https://crates.io/crates/fragpipe)
<!-- simit:badges:end -->

Bare-metal multiplayer and Android device test orchestration.

Fragpipe owns local, bare-metal, SSH, and Android-device smoke testing for
projects that can expose deterministic command-line launch modes. It does not
replace cluster harnesses such as steampipe; those remain better suited for VM,
Steam, and tournament orchestration.

## WebRTC 1v1

From a Chessbender checkout with `fragpipe.toml`:

```bash
fragpipe webrtc-1v1 --max-runs 1
```

The flow starts a local listening peer, waits for `WEBRTC_JOIN_ADDR=...`,
rewrites wildcard or loopback listen addresses to a reachable local IP, then
starts the remote joining peer over SSH.

## Android 1v1

`android-1v1` starts a desktop listening peer and launches an Android joining
peer from a configured APK:

```bash
fragpipe android-1v1 --config examples/chessbender.toml --max-runs 1
```

The Android config controls whether fragpipe boots an emulator or uses a
physical device selected by `adb_serial`. Physical-device runs also require a
desktop `local_ip` that the device can reach. Emulator and device APK build
commands may differ. Fragpipe installs and clears app data before every run,
pushes the rendezvous file, captures logcat, and records per-run artifacts.

## Android UI

`android-ui` launches the configured full app, waits for its exact configured
package/activity component to become resumed foreground, and requires a decoded, non-uniform landscape
screenshot. It retries readiness probes until the configured timeout, then
stores the screenshot, logcat, and a JSON report:

```bash
fragpipe android-ui --config examples/chessbender.toml --max-runs 1
```

Use `android-doctor` to validate SDK build-tools/aapt2, adb/device or AVD readiness, APK
existence, and the APK's actual package/activity metadata before running a loop:

```bash
fragpipe android-doctor --config examples/chessbender.toml
```

## MCP and Codex Plugin

Fragpipe also ships a `fragpipe-mcp` binary plus Codex plugin metadata. The MCP
server exposes the same fix-loop surface for native WebRTC, Android WebRTC,
Android UI, Android doctor, and cross-platform matrix runs.

Run the packaged MCP server with:

```bash
nix run git+ssh://git@codeberg.org/caniko/fragpipe.git#fragpipe-mcp
```

The plugin manifest lives at `.codex-plugin/plugin.json`, the MCP server config
lives at `.mcp.json`, and fix-loop skills live under `skills/`.
