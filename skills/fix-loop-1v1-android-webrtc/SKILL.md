---
name: fix-loop-1v1-android-webrtc
description: Run desktop plus Android test-peer WebRTC Direct 1v1 tests through fragpipe until repeated games pass.
user-invocable: true
---

# Fragpipe Android WebRTC 1v1 Fix Loop

Use the `fragpipe` MCP server's `repeat_android_1v1` tool.

Required parameters:
- `workdir`: project checkout containing the Android fragpipe config.
- `config`: Android fragpipe config, for example `dev/fragpipe.android.toml.example`.
- `max_runs`: current fix-loop tier run count.
- `timeout`: per-run timeout, usually `300`.

For physical devices, include `device=true` and `adb_serial`.

Preflight with `android_doctor`. If doctor fails, fix the SDK, adb, APK, AVD/device, package, or activity prerequisite before running the loop.
