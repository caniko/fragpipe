---
name: fix-loop-cross-platform-1v1
description: Run native WebRTC, Android WebRTC, and Android UI fragpipe cells as a cross-platform smoke matrix.
user-invocable: true
---

# Fragpipe Cross-Platform 1v1 Fix Loop

Use the `fragpipe` MCP server's `repeat_cross_platform_1v1` tool.

Required parameters:
- `workdir`: project checkout containing fragpipe configs.
- `native_config`: native WebRTC config, usually `fragpipe.toml`.
- `android_config`: Android config with test-peer and UI APK settings.
- `max_runs`: current fix-loop tier run count.
- `timeout`: default per-run timeout, usually `300`.

Run fail-fast first. If only one cell fails, switch to the focused WebRTC, Android WebRTC, or Android UI skill and fix that path before returning to the matrix.
