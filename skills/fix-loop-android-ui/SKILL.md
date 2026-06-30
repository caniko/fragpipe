---
name: fix-loop-android-ui
description: Run Android full-app UI launch, landscape assertion, and screenshot tests through fragpipe until repeated runs pass.
user-invocable: true
---

# Fragpipe Android UI Fix Loop

Use the `fragpipe` MCP server's `repeat_android_ui` tool.

Required parameters:
- `workdir`: project checkout containing the Android fragpipe config.
- `config`: Android fragpipe config with full-app `ui_*` fields.
- `max_runs`: current fix-loop tier run count.
- `timeout`: per-run timeout, usually `120`.

The loop must launch the full Android app, avoid fatal logcat markers, assert landscape dimensions, and capture a nonblank PNG screenshot. For physical devices, include `device=true` and `adb_serial`.

If the captured PNG is nonblank but the task requires semantic visual judgement
(layout quality, clipped text, overlap, readability, or rubric pass/fail), use
the global `visual-rubric` skill after the fragpipe run. Fragpipe owns launch,
logcat, dimensions, and screenshot capture; `visual-rubric` owns screenshot
rubric evaluation and missing-artifact policy.
