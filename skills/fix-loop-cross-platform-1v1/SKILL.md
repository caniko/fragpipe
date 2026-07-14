---
name: fix-loop-cross-platform-1v1
description: Run native WebRTC, Android WebRTC, and Android UI fragpipe cells as a cross-platform smoke matrix.
user-invocable: true
---

# Fragpipe Cross-Platform 1v1 Fix Loop

Use the `fragpipe` MCP server's `repeat_cross_platform_1v1` tool.

The tool has no schema-required inputs. By default it runs three cells: native WebRTC, Android WebRTC, and Android UI. Cluster cells run only when `cluster_cells` is supplied. Set these inputs explicitly for a reproducible consumer-project run:

- `workdir`: project checkout containing configs and artifacts.
- `native_config`: native WebRTC config, usually `fragpipe.toml`.
- `android_config`: Android config with test-peer and UI APK settings.
- `max_runs`: current fix-loop tier run count.
- `stop_on_failure`: `true` for a fail-fast fix loop.

Omit the shared `timeout` to retain fragpipe's per-cell defaults of 300 seconds for native/Android WebRTC and 120 seconds for Android UI. If supplied, the same timeout overrides every enabled fragpipe cell. Optional `cluster_cells` each have their own timeout and transport/display inputs.

Preflight the Android config with `android_doctor` after staging every APK path it checks. Doctor requires SDK build-tools/aapt2 and verifies each APK's actual package and launchable activity against the config. For a physical-device matrix, supply `device=true`, `adb_serial`, and `android_local_ip`; configure both `device_apk_build_command` and `device_ui_apk_build_command` for the target ABI. Doctor rejects unspecified, loopback, multicast, link-local, broadcast, and emulator-only `10.0.2.2` addresses; only the real Android WebRTC cell proves that the selected desktop address is actually reachable from the device.

Use these explicit MCP calls for a physical-device matrix after staging both device APKs:

- Preflight: `android_doctor(workdir=".", config="fragpipe.toml", device=true, adb_serial="<serial>", local_ip="<HOST_LAN_IP>", dry_run=false)`
- Matrix: `repeat_cross_platform_1v1(workdir=".", native_config="fragpipe.toml", android_config="fragpipe.toml", device=true, adb_serial="<serial>", android_local_ip="<HOST_LAN_IP>", max_runs=TIER_RUNS, stop_on_failure=true)`

Omit the matrix `timeout` to retain the cell-specific defaults, and keep builds enabled so fragpipe selects both device-specific APK build commands.

Run fail-fast first. If only one cell fails, switch to the focused WebRTC, Android WebRTC, or Android UI skill and fix that path before returning to the matrix.

Fragpipe MCP marks the call as an error when any cell fails. Even on tool success, require `failed cells: 0`, each gameplay cell's configured pass markers, and Android UI evidence that the exact configured package/activity component was resumed foreground with a decoded non-uniform landscape screenshot/report artifact.

When the Android UI cell captures a screenshot and the remaining question is
semantic visual quality rather than launch/logcat/dimension correctness, use
the consumer project's screenshot-rubric skill for semantic evaluation.
