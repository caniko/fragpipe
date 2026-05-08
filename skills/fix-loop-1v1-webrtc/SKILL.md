---
name: fix-loop-1v1-webrtc
description: Run native WebRTC Direct 1v1 tests through fragpipe until repeated games pass.
user-invocable: true
---

# Fragpipe WebRTC 1v1 Fix Loop

Use the `fragpipe` MCP server's `repeat_webrtc_1v1` tool.

Required parameters:
- `workdir`: project checkout containing the fragpipe config.
- `config`: native fragpipe config, usually `fragpipe.toml`.
- `max_runs`: current fix-loop tier run count.
- `timeout`: per-run timeout, usually `300`.

Start with `dry_run=true` to verify command generation, then run the 1 / 3 / 5 / 10 ladder with `stop_on_failure=true`.

Treat desync, DAG violations, fatal markers, watchdog exits, and missing pass markers as real bugs in the consuming project or its networking setup. Do not weaken validators to pass the loop.
