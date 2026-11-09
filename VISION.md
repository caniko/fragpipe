# fragpipe vision

`fragpipe` owns local, bare-metal, SSH, and Android-device smoke testing for
projects that can expose deterministic command-line launch modes. It starts real
peers, on real hosts and real devices, waits for real readiness and pass
markers, and keeps the logs and JSON reports that prove what happened. Its first
consumer is a multiplayer game with WebRTC, direct-LAN, tournament, and Android
clients.

## Problem

Networked and mobile behaviour cannot be verified from unit tests alone. Cluster
harnesses such as steampipe are built for VM, Steam, chaos, and isolated-host
orchestration, which is heavier and less faithful than launching two real peers
over the transport players actually use. Doing that by hand loses the evidence
and leaves remote processes behind when a run fails.

## Principles

- Test the real transport: a local listening peer plus a joining peer over SSH,
  or a desktop peer plus an Android APK on an emulator or physical device.
- Require explicit readiness and pass markers in the logs before a run counts.
- Preserve evidence per run: the local listener log, remote log tail, per-player
  logs, and a JSON result report under the configured artifact directory.
- Clean up only your own processes. Remote runs get per-peer PID files, and
  tournament players get distinct PID and log files.
- Keep remote execution narrow. Restricted SSH peers authenticate through a
  forced command that accepts typed lifecycle requests only; rsync uploads are
  write-only, cannot delete, and stay inside the configured root, and the gate
  rejects interactive shells, arbitrary commands, path traversal, forwarding,
  and unknown protocol fields.
- Let the consuming project own the launch contract: `fragpipe.toml` supplies
  host/join arguments, addresses, APK details, and artifact directories, while
  fragpipe only executes them.
- Do not accept a visual fixture merely because it has a name; the app must
  acknowledge the fixture and the downstream rubric needs its evidence manifest.

## Scope

The `webrtc-1v1`, `direct-1v1`, `direct-tournament`, `android-1v1`, `android-ui`,
and `android-doctor` runners; the `ssh-gate` forced-command surface; Android
worker slots leased over loopback-only SSH tunnels with ordered
`FRAGPIPE_ANDROID_SERIAL_*` environment variables; the `fragpipe-mcp` binary
with Codex plugin metadata and fix-loop skills; and the shared config, process,
log-watch, and SSH plumbing.

## Non-goals

It does not replace steampipe or become a general VM/Steam/chaos orchestrator;
Steam remains a separate cluster workflow. It does not build project APKs or
own game configuration. The fix loop is not a substitute for the consuming
project's own semantic visual rubric.

## Direction

Runner coverage is the active edge: the tournament runner and Android worker
slots were added most recently, and the same surface is exposed through the MCP
server for agent-driven fix loops. Recent infrastructure work added flake-input
checks against retired forge hosts.

## Place in the fleet

Canix pins the fragpipe flake, provisions the Atlas-to-nomad reverse-test SSH
key through its jarvis module, and writes `fragpipe/workers.toml` from
`canixLib.androidWorkers` for the Atlas user. It is also referenced from canix
planning notes for autonomous QA and visual hardware work.
