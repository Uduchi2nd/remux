#!/bin/bash
# (Re)create the dubmux container on the seedbox. Rootless podman, pasta
# networking needs a 12000+ port; data (audio cache, matches, HLS) lives in
# ~/dubmux/data on the array. Restart policy only covers process exits —
# like nzbdav, a SIGTERM from Whatbox maintenance leaves it stopped, so the
# usenet watchdog's pattern (podman start if not running) applies.
set -euo pipefail
cd "$HOME/dubmux"
mkdir -p data
podman rm -f dubmux >/dev/null 2>&1 || true
# --network=host: rootless pasta can't hairpin to the seedbox's own public
# hostname, and vnphim's proxied (kkphim/ophim) segments go through
# mediaflow-vn.geniallark.box.ca, i.e. this very box. Host networking also
# skips pasta's port restrictions; uvicorn binds 12500 directly.
podman run -d --name dubmux --restart unless-stopped \
  --network=host \
  -v "$HOME/dubmux/data:/data:Z" \
  --cpus 8 \
  localhost/dubmux:latest
for i in $(seq 1 20); do curl -fsS -m 3 http://127.0.0.1:12500/health 2>/dev/null && echo && exit 0; sleep 1; done
echo "dubmux did not answer /health within 20 s" >&2; exit 1
