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
3. **mux** (`/mux/<dub>/<hq>/master.m3u8`): one `ffmpeg -c copy` pass into
   6 s MPEG-TS segments (`-hls_playlist_type event`, dub delayed by the
   measured lag). Segments are served as they land; a seek past the produced
   range waits up to 25 s. On completion the playlist becomes VOD and stays
   cached (30 days, 300 GB budget, LRU). Embedded subtitles are dropped
   (MPEG-TS cannot carry text tracks); remux keeps serving addon/vnphim
   external subtitles.

## Operations
- Deploy: copy `dubmux.py server.py Containerfile run.sh` to seedbox
  `~/dubmux/`, `podman build -t localhost/dubmux:latest -f Containerfile .`,
  `./run.sh`. Data in `~/dubmux/data/{audio,match,hls}`.
- Debug: `GET /health`, `GET /jobs` (in-memory job states, errors),
  `GET /status/<dub>/<hq>`, `podman logs dubmux`.
- Restart policy only survives process exits; a Whatbox maintenance SIGTERM
  leaves it stopped (same as nzbdav) — `podman start dubmux`.
- Cold first play of an episode: ~20 s of preparation. remux waits up to
  `DUBMUX_PREPARE_WAIT_SECS` (12) in PlaybackInfo, so the rows usually appear
  on the second request (opening the episode triggers the first).
