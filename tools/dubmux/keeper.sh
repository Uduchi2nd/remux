#!/bin/bash
# Keeps the dub muxer (container dubmux, :12500) up. Rootless podman has no
# systemd restart and "unless-stopped" does not revive a container stopped by a
# SIGTERM (Whatbox maintenance 2026-09-29 ~23:59 UTC left it down while every
# other keeper-managed container came back). A running-but-unhealthy muxer is
# restarted only after 2 failed checks in a row (a restart cuts playback).
cd "$HOME/dubmux" || exit 1
st=$(podman inspect -f "{{.State.Status}}" dubmux 2>/dev/null)
if [ "$st" != "running" ]; then echo "$(date -u +%FT%TZ) state=$st -> start" >> keeper.log; podman start dubmux >> keeper.log 2>&1; rm -f .keeper-fails; exit 0; fi
code=$(curl -s -o /dev/null -w "%{http_code}" -m 30 http://127.0.0.1:12500/health)
if [ "$code" = "200" ]; then rm -f .keeper-fails; exit 0; fi
n=$(( $(cat .keeper-fails 2>/dev/null || echo 0) + 1 )); echo $n > .keeper-fails
[ "$n" -lt 2 ] && exit 0
if [ -f .keeper-restart ] && [ $(( $(date +%s) - $(cat .keeper-restart) )) -lt 300 ]; then exit 0; fi
date +%s > .keeper-restart; rm -f .keeper-fails
echo "$(date -u +%FT%TZ) health=$code twice -> restart" >> keeper.log; podman restart dubmux >> keeper.log 2>&1
