# fragpipe

<!-- simit:badges:start -->

[![CI](https://img.shields.io/badge/CI-drift-2088ff)](.forgejo/workflows/ci.yaml) [![Nix](https://img.shields.io/badge/Nix-managed-5277c3)](flake.nix) [![crates.io](https://img.shields.io/badge/crates.io-ready-f46623)](https://crates.io/crates/fragpipe)

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

## Direct LAN 1v1

`direct-1v1` runs a physical host-to-host UDP game. It deploys the configured
binary and assets to the named SSH peer, starts the local UDP listener, waits
for its readiness marker, launches the remote joiner, and requires the pass
marker in both logs:

```bash
fragpipe direct-1v1 --config fragpipe.toml --remote nomad --headless --max-runs 1
```

The game config owns the host/join arguments and direct-link address. Fragpipe
supports the LAN transport here; Steam remains a separate cluster workflow.
Remote processes receive a per-peer PID file so a failed run can be stopped
without killing unrelated games.

Each non-dry run also preserves `logs/fragpipe/direct-1v1/run-NN/` with the
local listener log, remote log tail, and JSON result report. Override this
directory with `[direct].artifact_dir` when a project keeps evidence elsewhere.

### Restricted SSH peers

Set `restricted = true` on a remote that authenticates through an OpenSSH
forced command. Restricted peers send typed lifecycle requests to
`fragpipe ssh-gate`; rsync uploads are write-only, cannot delete, and remain
inside the configured root.

```toml
[[remote]]
name = "atlas"
host = "regicide-atlas"
remote_dir = "/var/lib/regicide-peer/fragpipe"
restricted = true
```

The authorized key's forced command fixes every server-side path:

```text
restrict,command="fragpipe ssh-gate --root /var/lib/regicide-peer/fragpipe --binary chessbender-game --log-file game.log --pid-file .fragpipe.pid --rrsync /run/current-system/sw/bin/rrsync" ssh-ed25519 ...
```

Use an absolute path to the `rrsync` executable, not `rsync`. The gate rejects
interactive shells, arbitrary commands, path traversal, forwarding, and
unknown protocol fields.

## Android 1v1

`android-1v1` starts a desktop listening peer and launches an Android joining
peer from a configured APK:

```bash
fragpipe android-1v1 --config examples/chessbender.toml --max-runs 1
```

The Android config controls whether fragpipe boots an emulator or uses a
physical device selected by `adb_serial`. The APK build command is project
specific; fragpipe only runs it, installs the APK, pushes the rendezvous file,
captures logcat, and records per-run artifacts.

## Android UI

`android-ui` launches the configured full app, waits briefly for startup,
asserts that the Android display is landscape, captures a screenshot, and
stores logcat plus a JSON report:

```bash
fragpipe android-ui --config examples/chessbender.toml --max-runs 1
```

For the semantic visual matrix, set `android.visual_fixtures` and select
`all` or a glob. Each fixture is written to `launch_config_path` and the
app must acknowledge it before fragpipe accepts the screenshot:

```bash
fragpipe android-ui --config examples/chessbender.toml --visual-fixtures all
fragpipe android-ui --config examples/chessbender.toml --visual-fixtures 'battle_*'
```

A fixture is not considered covered merely because it has a name: the app
acknowledgement is required. Optional `android.image_validator` is an argv
template with `{image}` and `{fixture}` tokens, run after each screenshot.

Use `android-doctor` to validate SDK, adb, APK, target, package, and activity
configuration before running a loop:

```bash
fragpipe android-doctor --config examples/chessbender.toml
```

## MCP and Codex Plugin

Fragpipe also ships a `fragpipe-mcp` binary plus Codex plugin metadata. The MCP
server exposes the same fix-loop surface for direct LAN, native WebRTC, Android
WebRTC, Android UI, Android doctor, and cross-platform matrix runs.

Run the packaged MCP server with:

```bash
nix run git+ssh://git@codeberg.org/caniko/fragpipe.git#fragpipe-mcp
```

The plugin manifest lives at `.codex-plugin/plugin.json`, the MCP server config
lives at `.mcp.json`, and fix-loop skills live under `skills/`.
