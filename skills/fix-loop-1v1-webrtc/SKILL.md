---
name: fix-loop-1v1-webrtc
description: Run native WebRTC Direct 1v1 tests through fragpipe until repeated games pass.
user-invocable: true
---

# Fragpipe WebRTC 1v1 Fix Loop

Use the `fragpipe` MCP server's `repeat_webrtc_1v1` tool.

The tool has no schema-required inputs. Its defaults are the MCP server cwd, `fragpipe` from `PATH`, fragpipe's default `fragpipe.toml`, one run, a 300-second per-run timeout, and fail-fast behavior. Set these explicitly for a reproducible consumer-project run:

- `workdir`: project checkout containing the config and artifacts.
- `config`: native fragpipe config, usually `fragpipe.toml`.
- `max_runs`: current fix-loop tier run count.
- `timeout`: per-run timeout, usually `300`.
- `stop_on_failure`: `true` for the fix loop.

Start with `dry_run=true` to verify command generation, then run the 1 / 3 / 5 / 10 ladder with `stop_on_failure=true`.

The consumer's configured build profile must actually enable its automation CLI and headless path. In Chessbender that is the root `dev` feature; building only `game-runtime` accepts the process launch but ignores `--headless` and the auto-WebRTC arguments, leading to a misleading Winit/display panic before networking starts.

Fragpipe MCP propagates a nonzero child exit as a tool error. Even on tool success, require the returned fragpipe summary to report `max_runs/max_runs passed, 0 failed`, then verify the consumer project's configured pass marker in both peer logs. Treat desync, DAG violations, fatal markers, watchdog exits, and missing pass markers as real bugs in the consuming project or its networking setup. Do not weaken validators to pass the loop.
