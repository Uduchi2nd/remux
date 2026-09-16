# Remote HLS compatibility and fork retirement

Last reviewed: 2026-09-15. This is a retirement plan, not a claim that stock upstream now passes.

## Ownership and source of truth

- Upstream: https://github.com/lostb1t/remux
- Compatibility branch: https://github.com/Uduchi2nd/remux/tree/remote-hls-sources
- Current baseline: upstream v0.31.0. Patch stack before this documentation: c9989c01.
- VNPHIM is the user's own project, not a fork: https://github.com/Uduchi2nd/vnphim/tree/kkphim-playlist-compat
- VNPHIM's INFUSE-COMPAT.md and HOTPHIM-COMPAT.md describe provider fixes and their verification.

Keep changes server-side. Retire the remux fork when stock upstream demonstrates equivalent behavior with the same VNPHIM deployment. A clean rebase, an empty cherry-pick, or a conflict is not proof of compatibility.

## What currently belongs in remux

| Change | Purpose | Retirement evidence |
| --- | --- | --- |
| e54e589e: remote HTTP source metadata in PlaybackInfo and item MediaSources | Describe addon streams as remote HTTP URLs rather than local /remux files; expose probed container, dimensions and streams at item level | Stock responses describe the correct selected source and clients start without requiring a previous playback/probe |
| e54e589e: inline static HLS response | Return the small playlist with HTTP 200, HLS content type, Accept-Ranges: none and streaming/chunked response instead of a 307 | Actual clients start, seek and resume on stock; an equivalent upstream response is acceptable if it works |
| e54e589e: remote master playlist routing | Avoid starting an unnecessary transcode for redirectable sources | Master route plays with no unintended transcode or extra segment relay |
| ee9e647c: HLS detection and request handling | Recognize probe-reported HLS and .m3u8/.m3u, including cached extensionless sources; remove Range/If-Range on upstream inline-HLS requests; normalize only successful 200 responses | Cold/warm and extensionless cases work; upstream errors remain errors rather than becoming a fake successful playlist |
| ee9e647c: early probe and audio metadata | Bound cold item probe to 15 seconds; expose codec/audio details before PlaybackInfo; use first actual audio stream when no default is flagged | Cold item details and PlaybackInfo provide usable audio metadata and first playback succeeds |
| ee9e647c: Fladder selected-source aliases | Clone the selected PlaybackInfo source to the remembered advertised version count for this client; count is scoped by user/device/item and kept seven days | Explicitly choosing the last VNPHIM version plays that exact source, including after an error; no silent fallback to the first version |
| 2bf241bb / c9989c01: build workflow | Preserve the compatibility stack and build the canonical branch safely | Replace custom build/deploy workflow only after official-image acceptance |

The original commits bundle multiple behaviors. If upstream fixes only some, split or reduce the remaining patch rather than discarding the whole commit blindly.

## What stays in VNPHIM

Provider domain discovery, Hotphim title/episode/subtitle-label handling, cache expiry, stream URL generation, HLS playlist normalization, segment extension hints and readable filename fallbacks belong in VNPHIM.

Examples include explicit .m3u8 routes; final ext=.ts hints for compatible segment handling; short negative reachability caches; Hotphim's mflix.store migration and Viesub normalization; and named playlist routes that preserve old token routes.

These fixes cannot, by themselves, change the Jellyfin item/PlaybackInfo documents that remux generates. Playlist preparation could be done in VNPHIM, but removing remux's API changes requires upstream support or a separate Jellyfin API gateway. Do not add a gateway solely to avoid carrying a small remux patch.

The Infuse episode-name incident was resolved by turning OFF Show Filenames. It is not evidence of a remux metadata defect. Keep VNPHIM's readable filename fallback as requested.

## Acceptance matrix

Test the same VNPHIM version with both patched remux and isolated stock upstream. Record upstream tag/commit, client version, title/episode, exact selected source ID, cold/warm state, result and redacted evidence.

| Case | Previous evidence and required checks |
| --- | --- |
| Pursuit of Jade, KKPHIM via VNPHIM | Physical Infuse playback verified for episodes 1 and 4 after VNPHIM's segment hint fix. Test the last VNPHIM version explicitly; default TB playback is not a substitute |
| Fladder version selection | API alias checks passed for warm episode 8 and cold episode 9; full post-deploy Fladder playback was not conclusively verified. Require real playback and prove no fallback |
| Chrome Jellyfin web | Earlier KKPHIM episodes 9 and 12 played and sought. Repeat using the candidate stock server |
| Pham Nhan Tu Tien, HH3D episode 191 | Earlier Fladder success and later probe success; current physical Infuse playback not checked. Verify real playback; distinguish CDN CORS errors from remux defects |
| Against the Current, Hotphim episode 12 | Discovery, probe and short audio/video decode passed. Physical-client playback still needs verification. At last check this episode had subtitles but no dub |
| The Early Spring, Hotphim episode 4 | Search/discovery and subtitle/dub probes passed. Physical-client playback still needs verification |
| Legacy and named VNPHIM playlists | Both returned valid HLS and named route probed successfully. Preserve both routes |

For each applicable case:
1. Start a fresh episode/source and repeat warm. Measure cold-state behavior rather than assuming a restart clears all caches.
2. Verify video and audio, at least three minutes of advancing playback, forward seek and resume.
3. Confirm the requested provider/source ID in requests or logs. A player recovering by choosing another source is a failure for this test.
4. Exercise item details, PlaybackInfo, static stream and master routes as applicable, including Range requests and a controlled unavailable origin.
5. Confirm home/seedbox traffic remains small playlist/control traffic. Inspect segment destinations and byte counts; DirectPlay alone does not prove this. Existing Vietnam MediaFlow routes can still carry segment bandwidth.
6. Keep URLs, install blobs, passwords and tokens out of committed logs.

Cache notes: VNPHIM stream listings have a short cache, successful reachability checks may persist six hours, failures use about 60 seconds after the fixes, and AIO/client caches are separate. Refresh provider listings or use distinct episodes and record which layers were cleared.

## Safe retirement procedure

1. Choose a newer stable upstream tag. A nightly is useful for diagnosis, but is not automatically a production recommendation.
2. Back up configuration and data. Launch the official image on an isolated port with a COPY of the database, never the live database. Match web assets to the server version and account for schema migrations.
3. Run the matrix above against stock and the existing patched deployment with identical VNPHIM configuration.
4. If any required behavior fails, retain the relevant patch and record the failing case. Reduce the patch stack only with evidence.
5. Once required cases pass, deploy the matching official image and remove the custom remux-server binary volume override. Preserve backups and a rollback path compatible with database migrations; never blindly downgrade a migrated production database.
6. Repeat playback checks in production. Update this document and Claude memory with the accepted tag, exact evidence and date.
7. Archive the compatibility branch and build instructions for reference; do not delete the history. VNPHIM changes remain independently maintained.

Current deployment tooling: seedbox ~/remux-build/build.sh [upstream-tag], nimo /root/remux-patch/deploy.sh. The build script preserves the patch stack, creates a backup branch before rebasing and refuses tracked uncommitted edits. Deployment must stage and swap the binary with container recreation and rollback checks; do not overwrite a running bind-mounted executable.

## Upstream activity snapshot

Verified 2026-09-15 using GitHub releases and commits:
- Latest stable: v0.31.0, September 11.
- Eight stable releases from August 15 through September 11 (v0.25.0 through v0.31.0, including v0.28.1).
- The first API page already contained 100 commits since August 16: at least 100, not an exact total.
- New commits continued through September 15.
- Related post-release work includes probe fallback on subsequent stream requests (#464), preserving HTTP Range for MKV direct streams (#440), HTTP-only reconnect options (#478), and HEVC sample-entry selection from client DeviceProfile (#413).

Sources: https://github.com/lostb1t/remux/releases and https://github.com/lostb1t/remux/commits/main/

Assessment: active development with frequent releases and relevant playback work. None of those changes alone proves our remote-HLS or Fladder workaround can be removed. No upstream upgrade or patch retirement was performed for this documentation update.
