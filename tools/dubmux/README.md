# dubmux — Vietnamese dub on high-quality sources

Seedbox service (rootless podman, host networking, port 12500, public
`https://dubmux.geniallark.box.ca` via the Whatbox Apps panel) that pairs a
vnphim Thuyết Minh/Lồng Tiếng stream with a debrid/usenet release and serves a
stream-copy HLS mux: HQ video + dub as default audio + every original audio
track. remux (`services/dubmux.rs`, config `DUBMUX_URL` / `DUBMUX_PUBLIC_URL`
/ `DUBMUX_PREPARE_WAIT_SECS`) creates "[+VN dub · <provider>]" stream rows in
front of the source list for every accepted pair.

## Pipeline (per dub × HQ pair, all on the seedbox)
1. **extract** (`dubmux.py extract`): parse the HLS playlist (byterange aware),
   fetch every segment with 128 parallel range requests (6 when segments go
   through the VN MediaFlow), demux the AAC with `-c copy` to `audio/<id>.m4a`.
   Đầu Xuân Tươi Sáng E02 hotphim: 484 segments / 819 MB in 4.9 s, 8.9 s total
   (416 s when ffmpeg streamed it serially).
2. **match** (`/prepare`): resolve the HQ URL to its final CDN location (addon
   playback URLs are redirectors ffmpeg cannot seek through), gate on duration
   (±1.5 s), then cross-correlate 120 s mono 8 kHz windows at 90 s / midpoint /
   end-150 s (numpy FFT; envelope fallback). Accept when all peak/rms ≥ 8 and
   the lags agree within 0.15 s. ~11 s. Result cached in `match/`.
2b. **piecewise** (`align.py`, when the duration gate fails by ≤ 180 s): VN
   encodes are the same episode minus blocks (streamer ident, opening
   credits, tail preview), so the lag is piecewise constant. The HQ audio is
   fetched once into a local .mka (debrid CDNs 429 on dozens of ranged
   seeks); 60 s coarse windows are grouped into runs of constant lag (single
   weak windows are noise, dropped), each boundary is pinned to 0.5 s by
   comparing the two candidate lags on 3 s windows, and one AAC track is
   rendered on the video's clock: dub inside the runs, the release's own
   audio in the gaps. Pursuit of Jade E01 (kkphim dub 2783 s vs NF 2820 s):
   dub[0,178) at −5.92 s, dub[178,end) at −33.44 s, original audio for the
   5.9 s ident, the 27.5 s credits and the 3 s tail. The mux then uses
   `match/<pair>.aligned.m4a` with no offset. Sign convention:
   `video(t) <-> dub(t + lag)` (verified on a synthetic delay).
3. **mux** (`/mux/<dub>/<hq>/master.m3u8`): one `ffmpeg -c copy` pass into
   6 s MPEG-TS segments (`-hls_playlist_type event`, dub delayed by the
   measured lag). Segments are served as they land; a seek past the produced
   range waits up to 25 s. On completion the playlist becomes VOD and stays
   cached (30 days, 300 GB budget, LRU). Embedded subtitles are dropped
   (MPEG-TS cannot carry text tracks); remux keeps serving addon/vnphim
   external subtitles.

## Incidents / rules learned
- The mux routes must answer HEAD (FastAPI `@app.get` does not): remux's
  liveness check HEADs the master playlist, a 405 counted as "dead", remux
  fell back to a live ffprobe of the mux (no text tracks) and
  `save_probe_data` overwrote the row's synthesized probe — which is why
  dub rows lost their embedded subtitles and why probes kept starting
  muxes. Routes are `api_route(methods=["GET","HEAD"])` now; HEAD never
  starts a mux.
- Background stream probing (remux `probe_background_streams`) must skip
  `[+VN dub]` rows: an ffprobe on the master playlist starts a full-episode
  mux, and one prefetch sweep produced 108 muxes / 298 GB. HEAD requests
  never start a mux; `DUBMUX_HLS_BUDGET_GB` is 150 and retention also runs
  at startup (`POST /sweep` applies it on demand).
- Players must receive a finished VOD playlist: VidHub treats the
  in-progress EVENT playlist as live (length 0, no seeking, stops after the
  first segments). Master/index GETs wait up to `DUBMUX_MASTER_WAIT_S`
  (50 s) for `.done` and rewrite the type to VOD; remux POSTs
  `/mux/{dub}/{video}/start` for every row at playback time so the wait
  is normally zero. Cold 1.5–2.3 GB releases mux in 7–40 s.
- Never edit repo files while a build chain is still in its format/copy-back
  step: the copy-back clobbered a later edit once (the 2-previous walk).
- Piecewise acceptance must be judged on confident windows, not on run
  extent: the last run used to stretch to the end of the dub, so 3 confident
  windows out of 46 (Pursuit of Jade E01 kkphim × DDHDTV 2160p — the
  release's audio does not match past the opening) became one constant
  offset that was wrong from ~22:00. `analyse()` now needs ≥ 85 % of the
  coarse windows confident and inside accepted runs, and only extends a run
  to the end when its evidence reaches there. Real matches score ~100 %,
  bad pairs ≤ 50 % — there was nothing in between across 142 records.
- A dub row's path must never be rewritten to remux's subtitle-ready gate
  route: the master is then served inline by remux and its relative
  `index.m3u8` resolves against the wrong host (VidHub: row never starts).
  The HQ release's embedded text tracks on a dub row are presented as
  external (`Path <lang>.vtt`) so VidHub/Infuse list them.

## Operations
- Deploy: copy `dubmux.py server.py Containerfile run.sh` to seedbox
  `~/dubmux/`, `podman build -t localhost/dubmux:latest -f Containerfile .`,
  `./run.sh`. Data in `~/dubmux/data/{audio,match,hls}`.
- Debug: `GET /health`, `GET /jobs` (in-memory job states, errors),
  `GET /status/<dub>/<hq>`, `podman logs dubmux`.
- Restart policy only survives process exits; a Whatbox maintenance SIGTERM
  leaves it stopped (same as nzbdav) — `podman start dubmux`.
- HEAD on master.m3u8 never starts a mux (remux liveness checks); only a
  GET does. `DUBMUX_MATCH_SLOTS` (3) bounds concurrent matches. The
  container runs with `--network=host` (pasta cannot hairpin to the
  seedbox's own public hostname used by vnphim's VN-proxied segments).
- Cold first play of an episode: ~20 s of preparation. remux waits up to
  `DUBMUX_PREPARE_WAIT_SECS` (12) in PlaybackInfo, so the rows usually appear
  on the second request (opening the episode triggers the first).

## Not here: yanhh3d segment proxying (2026-09-26)

A `/relay/` passthrough for yanhh3d's CDN segments lived in this service
for a few hours on 2026-09-26 and was replaced the same day by a MediaFlow
Proxy Light on the seedbox (`mediaflow-us.geniallark.box.ca`, container
`mediaflow-us`, port 12888, `~/mediaflow-us/keeper.sh`), which vnphim
wraps segments with (`yanhh3d.mediaflow_us`). Reason for proxying at all:
the donghuavip CDN pulls a cold segment through Cloudflare's LAX edge at
100 KB/s–1.3 MB/s (erratic) but through IAD, where the seedbox lands, at
1.6–3 MB/s; per-POP cache, no tiered cache. Keep this service dub-only.

HLS retention budget is 500 GB (`DUBMUX_HLS_BUDGET_GB`, raised from 150
on 2026-09-26 at the user's request; ~2.3 GB per muxed episode).
