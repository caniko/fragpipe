---
name: fix-loop-1v1-direct-lan
description: Run repeated physical-peer LAN/UDP 1v1 games through Fragpipe until both logs pass.
---

# Direct LAN 1v1 fix loop

Use the `fragpipe` binary from the project checkout:

```bash
fragpipe direct-1v1 --config fragpipe.toml --remote <peer> --headless --max-runs 1 --timeout 300
```

The configured remote must be reachable over SSH and the host must be the
peer's direct-link address. The runner deploys the built binary and assets,
starts the local UDP listener, launches the remote joiner, and requires the
configured pass marker in both logs.

For a stability ladder, run 1, then 3, then 5, then 10 consecutive games. Stop
at the first failure and preserve the run artifacts. A pass requires `GAME OVER`
from both peers with no configured fatal marker, no early process exit, and no
timeout. Confirm the logs show the expected direct `/udp/<port>/quic-v1` path;
do not classify this transport as WebRTC or Steam.

Use `--dry-run` first when validating a new remote entry. Use `--no-build` and
`--no-deploy` only after the deployed binary and assets are known current.
