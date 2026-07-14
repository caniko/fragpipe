---
name: fix-loop-android-ui
description: Run Android full-app UI launch, landscape assertion, and screenshot tests through fragpipe until repeated runs pass.
user-invocable: true
---

# Fragpipe Android UI Fix Loop

Use the `fragpipe` MCP server's `repeat_android_ui` tool.

The tool has no schema-required inputs. Its defaults are the MCP server cwd, `fragpipe` from `PATH`, fragpipe's default `fragpipe.toml`, one run, a 120-second per-run timeout, and fail-fast behavior. Set these explicitly for a reproducible consumer-project run:

- `workdir`: project checkout containing the Android config and artifacts.
- `config`: Android fragpipe config with full-app `ui_*` fields.
- `max_runs`: current fix-loop tier run count.
- `timeout`: per-run timeout, usually `120`.
- `stop_on_failure`: `true` for the fix loop.

Preflight with `android_doctor`, but build or stage every APK path that doctor checks first: doctor validates existing artifacts and never runs the configured APK build commands. It uses Android SDK build-tools `aapt2 dump badging` to require each APK's actual package and launchable activity to match the fragpipe config. Treat a missing APK or build-tools/aapt2 as a producer/environment failure, and treat metadata mismatch as either a wrong artifact or wrong config; repair the responsible producer/config and rerun doctor before the loop.

The loop must launch the full Android app, avoid fatal logcat markers, verify that the exact configured package/activity component is resumed foreground, and decode a landscape PNG with non-uniform visible pixels. Fragpipe retries foreground/screenshot readiness probes until the configured timeout. For a physical device, supply `device=true` and `adb_serial`; the shared doctor also validates the Android 1v1 address class, so pass its `local_ip` input when overriding the config to device mode. Doctor rejects emulator-only/non-routable address classes, while a real Android WebRTC run—not doctor—proves route reachability. Configure `device_ui_apk_build_command` to stage a device-ABI full-app APK; fragpipe selects it automatically. If a consumer config lacks that override, its generic UI build command must be device-compatible, or the consumer must explicitly pre-stage and use `no_build=true`.

Use these explicit MCP calls for the physical-device path after staging the device APKs:

- Preflight: `android_doctor(workdir=".", config="fragpipe.toml", device=true, adb_serial="<serial>", local_ip="<HOST_LAN_IP>", dry_run=false)`
- Loop: `repeat_android_ui(workdir=".", config="fragpipe.toml", device=true, adb_serial="<serial>", max_runs=TIER_RUNS, timeout=120, stop_on_failure=true)`

Keep builds enabled in the normal loop so fragpipe selects `device_ui_apk_build_command`; use `no_build=true` only after explicitly staging and validating the target-matching artifact.

Fragpipe MCP propagates a nonzero child exit as a tool error. Even on tool success, require `max_runs/max_runs passed, 0 failed`, then inspect the captured logcat, report, and screenshot artifacts.

If the captured PNG is structurally non-uniform but the task requires semantic visual judgement
(layout quality, clipped text, overlap, readability, or rubric pass/fail), use
the consumer project's screenshot-rubric skill after the fragpipe run. Fragpipe
owns launch, resumed-foreground verification, logcat, decoded landscape/non-uniform checks, and screenshot capture; the consumer project
owns semantic screenshot evaluation and missing-artifact policy.
