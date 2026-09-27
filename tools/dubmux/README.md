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

## Encrypted stream addresses → origin (2026-09-26)

vnphim now hands out opaque encrypted MediaFlow URLs, so the dub `url`
remux passes to `/prepare` cannot be unwrapped locally. `dubmux.dub_source`
asks vnphim (`GET /_internal/dub-source?u=…`, header `X-Vnphim-Key`; env
`DUBMUX_VNPHIM_URL`/`DUBMUX_VNPHIM_KEY` in `~/dubmux/.env`, loaded by
`run.sh --env-file`) for the origin master + Referer behind it and rebuilds
a MediaFlow-style wrap (`/proxy/stream?d=<origin>&api_password&h_Referer`,
password from `DUBMUX_MF_PASSWORD`) so the VN extractor unwraps it and pulls
from the origin as before, and the seedbox-side fallback still works.
`_join` resolves relative playlist URIs against the `d=` origin inside such
wraps (a plain urljoin produced `…/proxy/2000kb/hls/…` → MediaFlow 401 —
every extraction failed that way for ~20 minutes on 2026-09-26). Without
the lookup the muxer silently extracted whole dubs THROUGH the VN
MediaFlow (part of what blew its memory up to 800 MB that day).

## Priority queue (2026-09-26)

Extraction (2 slots, = what the VN extractor runs in parallel) and matching
(3 slots) are `PriorityGate`s: lowest `priority` first, FIFO on ties, and a
re-POST of `/prepare` with a better number bumps a queued job in place.
`GET /queue` shows occupancy and the waiting pairs. remux sends:

| caller | priority |
| --- | --- |
| playback request for the episode being played | 0 |
| on-play walk, next episode … 10th next, then 2 previous | 100 + position |
| hourly background refresh | 300 |
| (any) second dub provider / second-best release of an episode | base + 50 + rank |

so every episode's FIRST dub version in the walk is prepared before anyone's
second variant, and the series being watched never waits behind another
series' upkeep. Jobs submitted by older remux builds get 500.

### Priority order and slots (2026-09-27)
remux → muxer priorities: current episode (playback) 0, NEXT episode 2, item
opened in the app 10, the rest of the on-play walk 100+n (upcoming first,
previous last), background refresh 300, evaluation 400; a pair that is not
the best of its episode adds 50+rank, so every episode's first dub version
lands before any variant. Gates (`PriorityGate`): match 3 shared slots + 1
reserved for ≤99 + 1 express for ≤9 (playback/next only); extract 2+1+1; raw
copy (the full-release fetch) 2+1+1 → at most 4 releases downloading at
once. Running jobs are never preempted; priority only orders the waiting.

## Acceptance by time coverage (2026-09-26)

`align.py` accepts a piecewise match on either of two criteria: (a) ≥ 85 %
of the coarse windows are confident and inside runs, or (b) the runs span
≥ 85 % of the dub's timeline, every run has ≥ 2 confident windows, and no
run has more than 4 unconfident windows (240 s) between confident ones.
(b) exists for dialogue-sparse stretches — music, effects the dub re-mixed —
whose windows correlate with nothing although the offset is constant across
them (their lags come out random, i.e. they are true no-match windows, not
weak matches); a small cut inside such a gap would have split the run.
Queen of News E03 vs the DDHDTV 4K: 25/42 confident windows but 94 % time
coverage with a worst gap of 3 → accepted (`coverage_mode: "time"`).
Runs never extend past their last confident window (+60 s) unless that is
within two steps of the end, so (b) cannot be satisfied by a stretched
tail. 34 stored `reject:coverage` records were purged for re-evaluation.

## Aligner defects found by the 500-item evaluation (2026-09-27)

Three fixes in `align.py`, found by re-reading the coarse evidence of every
"no alignment accepted" item:
- A run with a POSITIVE lag (the dub starts before the video — longer VN
  streamer ident) was dropped because its `video_start` came out negative;
  the run is now clipped to dub time `[lag, …)`. 89 stored rejections had
  thrown away a solid match this way (LINK CLICK S02E02: 19/23 windows at
  +34.6 s, coverage reported 0.0).
- The lag search range was `skew + 15 s`; kkphim injects a ~30 s mid-roll
  ad around 15 min, so after it the true lag exceeded the range and
  correlation "died" (The First Frost E25, High School Return of a
  Gangster E05). The range is now at least 75 s (`MIN_MAX_LAG`).
- `MAX_SKEW` 180 → 360 s (Striking Rescue: 206 s of extra credits).
172 affected records were purged for re-evaluation.

## HQ candidate rule widened (2026-09-27)

The 500-item evaluation showed 55 of 59 "dub but no usable release" items
had releases that remux's rule rejected: usenet (NzbDAV) streams are named
like scene releases with NO file extension, and the rule demanded
`.mkv`/`.mp4`. Names are now accepted unless their extension is a
playlist/non-file kind (m3u8, ts, strm, avi, wmv, flv, iso, rar). vnphim's
own rows are excluded by release-name tokens (`.kkphim.`, `.ophim.`,
`.hotphim.`, `.yanhh3d.`, `ProxiedVN`, `.Vietsub.`) since their URLs are
opaque MediaFlow addresses now. Unit test
`hq_candidate_accepts_extensionless_usenet_names_and_rejects_vn_sources`.

## Two-pass mux, pre-screen, self-check (2026-09-27)

Pipeline per pair now: extract dub (VN) → **pre-screen** (8 × 60 s windows of
the release by byte range, ~1 % of the file; < 3 confident windows →
`reject:prescreen`, no download; ≥ 5 agreeing within 0.15 s and equal
durations → same-cut accept with that lag, no full read) → otherwise
**raw copy**: the release is stream-copied ONCE into `hls/raw-<hq>/`
(video + all original audio, shared by every dub of that release; for
interactive priorities it downloads in parallel with the extraction) →
piecewise alignment against `raw-<hq>/audio.mka` on local disk → **final
mux is a local remux** from the raw copy (18 s measured; falls back to the
remote release when no copy exists). Measured on Speed and Love E03 ×
TorBox 2160p: 202 s end to end (was 3–4.5 min sequential, 8–13 min under
the day's incidents).
Garbage rules: a rejected pair drops the raw copy at once unless another
job uses it or an accepted match for that release exists; raw copies go
`DUBMUX_RAW_KEEP_S` (2 h) after their last use, failed ones after 30 min,
and they count toward the HLS budget. `health` reports `raw_copies`.
Every finished mux runs `_verify_sync`: the dub track is cross-correlated
against the release's first audio track at three points; a confident
disagreement > 0.35 s marks the session `.failed` (`.verify` holds the
numbers). It caught a +1.6 s double clock shift in the first build.

## Cache redesign: durable audio + JIT playback (2026-09-27)
What is kept, and for how long (`Containerfile` env, LRU within budget):
- **Durable** (`audio/`, `match/`; 200 GB / 365 d, LRU by mtime, bumped on
  every use by `_touch_pair`): extracted dub tracks, match records, ALIGNED
  tracks (`<dub>__<hq>.aligned.m4a` + a `.json` sidecar listing every release
  the track is known to fit — one aligned track serves several releases via
  the pre-screen), and the release's **segment table**
  `segtab-<hq>.json` (`{"v":3,"starts":[true PTS of every raw segment],
  "last":dur,"origin":start_time}`).
- **Play cache** (`hls/<dub>__<hq>/`; 100 GB / 30 d, LRU by `.touched`):
  finished muxes, ONE version per pair. `hls/raw-<hq>/` local copies are
  staging (deleted 2 h after last use).
Playback is built **just in time** against the release (raw copy while it
exists, else the live debrid/usenet URL): `master.m3u8` answers in ~1 s with
a full **VOD** playlist derived from the segment table, and a producer
(`ffmpeg -copyts … -f segment -segment_times`) makes the segments from
wherever the player is. A far seek restarts the producer at that segment
(~0.5 s), and a **mover thread** moves each finished segment from the run's
own dir into the session, so served files are always whole and a killed
producer never truncates anything. When a run ends with holes (the viewer
seeked around) bounded fill runs complete the session, then the sync
self-check runs and the session becomes cache.
Hard-won ffmpeg facts (all measured, see `_jit_cmd`):
- `-segment_times` are RELATIVE to the run's first packet and indexed per
  run (`segment.c`: `end_pts = times[count] + reference_stream_first_pts`),
  not absolute, not by `segment_start_number`. Pass `starts[i] - T`.
- The table must be the real first PTS of each raw segment (parsed from the
  TS files, 64 KB reads): cumulative EXTINF sums drift (1.6 s over one
  file) and the cuts then miss keyframes.
- `-copyts` keeps original timestamps after an input `-ss`; `-start_at_zero`
  shifted a seeked run's clock by 1.6 s — never use it here. The alignment
  lag lives on the re-based clock, so the dub is offset by
  `origin - dub_start_time - lag` (`origin` = the release's container
  start_time) and always input-seeked so no packet lands before 0
  (avoid_negative_ts would otherwise shift a whole run).
- The self-check decodes both audio tracks with
  `aresample=first_pts=0` so they share the absolute clock; passes at
  -0.022 s (AAC priming).
- The hls muxer's own `hls_time` splitting is not reproducible across a
  restart; a finished session whose playlist does not list every segment
  with `#EXT-X-ENDLIST` (a legacy producer killed mid-playlist — the "0
  length" Love in the Clouds E1 row) is rebuilt on the next master GET
  (`_session_consistent`).
Matches prepared before tables existed (255 of 279 releases on 2026-09-27,
mostly evaluation runs) need a raw copy on first play: master starts it in
the background (deduplicated per release) and waits up to
`DUBMUX_MASTER_WAIT_S` (50) for the session to open; opening the episode
in remux (`/start`) makes that copy ahead of the first play.

## Ahead-of-time warming of trending Chinese/Korean titles (2026-09-27)
`tools/dubmux/warm.py` (lives at `/root/dubmux-warm/warm.py` on nimo, root cron
`10 */2 * * *`, log `warm.log`, state `state.json`): reads remux's promoted
libraries Hot Chinese Shows, Hot Korean Shows, Netflix South Korea Top 10,
Trending Shows/Movies, Hot Korean Movies (titles filtered to
`ProductionLocations` China/Hong Kong/Taiwan/South Korea, ≤100 titles), takes
the latest 10 AIRED episodes of each series (and the movies), and inserts them
into remux's persistent `background_stream_refresh_jobs` queue at priority 10
(below every viewer-driven job: 40/80/100/200). The queue worker refreshes
the episode's streams → "[+VN dub]" rows → muxer `/prepare` at muxer priority
300 (behind playback and the on-play walk) → dub audio aligned + segment
table in the durable cache; nothing is pre-muxed, so the play cache is not
touched. Pacing: ≤40 episodes per run, skipped while the muxer has >80
waiting pairs at priority ≤300 or remux still holds >20 warm jobs; an episode
is not re-queued within 7 days. DB writes go through a throwaway
`python:3.12-slim` container in LXC 111 (short WAL transactions beside the
running server; `ON CONFLICT DO NOTHING` so a viewer's job never loses its
priority). Env knobs: `WARM_MAX_TITLES`, `WARM_EPISODES`, `WARM_BATCH`,
`WARM_REPEAT_DAYS`, `WARM_MUXER_MAX_WAITING`, `WARM_REMUX_MAX_PENDING`.
Rough cost: ~2–4 GB of seedbox download per episode for the alignment's raw
copy (deleted 2 h later), ~100 MB kept per episode in the audio cache.
