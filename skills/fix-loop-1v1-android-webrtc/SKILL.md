---
name: fix-loop-1v1-android-webrtc
description: Run desktop plus Android test-peer WebRTC Direct 1v1 tests through fragpipe until repeated games pass.
user-invocable: true
---

# Fragpipe Android WebRTC 1v1 Fix Loop

Use the `fragpipe` MCP server's `repeat_android_1v1` tool.

The tool has no schema-required inputs. Its defaults are the MCP server cwd, `fragpipe` from `PATH`, fragpipe's default `fragpipe.toml`, one run, a 300-second per-run timeout, and fail-fast behavior. Set these explicitly for a reproducible consumer-project run:

- `workdir`: project checkout containing the Android config and artifacts.
- `config`: consumer-project Android fragpipe config.
- `max_runs`: current fix-loop tier run count.
- `timeout`: per-run timeout, usually `300`.
- `stop_on_failure`: `true` for the fix loop.

Preflight with `android_doctor`, but build or stage every APK path that doctor checks first: doctor validates existing artifacts and never runs the configured APK build commands. It uses Android SDK build-tools `aapt2 dump badging` to require the APK's actual package and launchable activity to match the fragpipe config. Treat a missing APK or build-tools/aapt2 as a producer/environment failure, and treat metadata mismatch as either a wrong artifact or wrong config; repair the responsible producer/config and rerun doctor before the loop.

For a physical device, supply all of `device=true`, `adb_serial`, and `local_ip`. Doctor rejects unspecified, loopback, multicast, link-local, broadcast, and emulator-only `10.0.2.2` addresses; that is a structural preflight, while a real Android WebRTC run proves that the selected desktop address is actually reachable from the device. Configure `device_apk_build_command` to stage a device-ABI APK; fragpipe selects it automatically for `device=true`. If a consumer config lacks that override, its generic `apk_build_command` must already be device-compatible, or the consumer must explicitly pre-stage and use `no_build=true`.

Use these explicit MCP calls for the physical-device path after staging the device APKs:

- Preflight: `android_doctor(workdir=".", config="fragpipe.toml", device=true, adb_serial="<serial>", local_ip="<HOST_LAN_IP>", dry_run=false)`
- Loop: `repeat_android_1v1(workdir=".", config="fragpipe.toml", device=true, adb_serial="<serial>", local_ip="<HOST_LAN_IP>", max_runs=TIER_RUNS, timeout=300, stop_on_failure=true)`

Keep builds enabled in the normal loop so fragpipe selects `device_apk_build_command`; use `no_build=true` only after explicitly staging and validating the target-matching artifact.

Fragpipe MCP propagates a nonzero child exit as a tool error. Even on tool success, require `max_runs/max_runs passed, 0 failed` in the returned summary and the configured pass marker in both the desktop-peer log and Android logcat. If doctor fails, fix the SDK, adb, APK, AVD/device, reachable address, or configured package/activity prerequisite before running the loop.
