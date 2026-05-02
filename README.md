# fragpipe

Bare-metal multiplayer test orchestration for Chessbender.

Fragpipe owns native WebRTC Direct smoke testing between real machines. It does
not replace steampipe; steampipe remains the Steam and VM-cluster harness.

## WebRTC 1v1

From a Chessbender checkout with `fragpipe.toml`:

```bash
fragpipe webrtc-1v1 --max-runs 1
```

The flow starts a local listening peer, waits for `WEBRTC_JOIN_ADDR=...`,
rewrites wildcard or loopback listen addresses to a reachable local IP, then
starts the remote joining peer over SSH.
