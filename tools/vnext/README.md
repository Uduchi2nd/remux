# vnext — dub audio extractor on the VN Proxmox (LXC 100)

kkphim/ophim segments are geo-blocked outside Vietnam; from the seedbox they
only reach through the VN MediaFlow over the VN home uplink (~2.4 MB/s, 6
workers on purpose), which made a kkphim dub extraction take ~10 min.
`vnext` runs next to MediaFlow in LXC 100 (`/opt/vnext`, docker compose,
port 19000, exposed tailnet-only as
`https://vietnam-home50btx-proxmox.buffalo-deneb.ts.net:8444` via
`tailscale serve` on the VN host). It takes vnphim's stripped playlist,
unwraps each MediaFlow-wrapped segment to its origin URL (+ the Referer
MediaFlow would inject), fetches with domestic bandwidth (64 connections,
measured 7.4 MB/s — kkphim caps per client, 24 connections gave 3.2 MB/s),
pipes the segments in order into `ffmpeg -i pipe:0 -vn -c:a copy` (nothing
large touches the 8 GB rootfs; `pct resize 100 rootfs +4G` was applied), and
serves the ~70 MB .m4a for 12 h.

The seedbox muxer (`tools/dubmux/dubmux.py::vn_extract`) delegates every
MediaFlow-proxied playlist here when `DUBMUX_VN_EXTRACTOR` is set, through
tailscaled's local CONNECT proxy (`DUBMUX_VN_PROXY`, default
`http://127.0.0.1:1055` — the seedbox tailscale is userspace-networking and
resolves MagicDNS only via that proxy), and falls back to the proxied
fetch if the extractor is unreachable.

Deploy: copy `server.py dubmux.py Dockerfile docker-compose.yml` to LXC 100
`/opt/vnext/` (`pct push`), `docker compose build && docker compose up -d`.
Debug: `GET /health`, `GET /extract/<id>`, `docker logs vnext`.
