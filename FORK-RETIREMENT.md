# Remote HLS compatibility and fork retirement

Last reviewed: 2026-09-20. This is a retirement plan, not a claim that stock upstream now passes.

Background preparation, cache freshness, durable jobs, subtitle processing ownership, rollout tests, and their upstream retirement criteria are designed in [BACKGROUND-PREPARATION-DESIGN.md](BACKGROUND-PREPARATION-DESIGN.md). This is a proposal only; it does not describe deployed behavior.

Local implementation is in progress: stream-list job queue and seven-day aligned-subtitle disk cache are not deployed. Remaining provider-specific background checks and verification gaps are listed in the design document.

## Ownership and source of truth

- Upstream: https://github.com/lostb1t/remux
- Compatibility branch: https://github.com/Uduchi2nd/remux/tree/remote-hls-sources
- Current baseline: upstream v0.33.0 (rebased 2026-09-20; earlier baselines v0.31.0, v0.32.0). v0.33.0 adds `AddonFetchTimeoutSecs` (default 5 s) — production sets it to 30 because UsenetStreamer's triage takes 3–20 s; re-check the value after any upgrade.
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
| ac6d40ef: probe fallback cascade (2026-09-20) | PlaybackInfo without a chosen version probed only the first stream and fell back solely to same-resolution siblings; the tag comes from the filename, which Torrentio-style names lack, so one dead debrid link became a 500 for the whole episode ("tried 1 of 4 streams"). Matching streams are still tried first, then the rest, bounded by MaxProbeFallbackStreams | Stock PlaybackInfo (no MediaSourceId) returns a playable source when the first listed stream is dead but another listed stream probes; upstream #464-style "probe fallback on subsequent requests" alone is not equivalent |
| probe failure memory (2026-09-20, commit after ac6d40ef) | A stream whose probe just failed is skipped for 10 minutes while any candidate without a recent failure is still ahead (last resort otherwise; success clears it). Without it every PlaybackInfo re-probed the dead TorBox link for the full timeout and Infuse/VidHub gave up first | Stock returns a playable source quickly on the second and later PlaybackInfo when the first listed stream is dead |
| addon subtitles in item documents (2026-09-20) | `EnableSubtitlesDetail` (dashboard setting, previously unused) makes `item_for_user` inject AIOStreams/vnphim subtitle tracks into the item's MediaSources, exactly as PlaybackInfo does; Infuse and VidHub build their subtitle picker from the item document, so external Vietnamese tracks were invisible on sources without embedded ones. Cache-backed, ~4 s cold per item, 0.04 s warm | Stock item documents list addon subtitle tracks (or both clients pick them up from PlaybackInfo) |
| addon subtitles at item top level (2026-09-20) | Jellyfin mirrors the primary source's streams at `BaseItemDto.MediaStreams`; Infuse can use `MediaSources[n]`; exposing both levels alone was insufficient for Vidhub 3.0.6 (see the Path check below). External addon subtitles are now mirrored there too and `HasSubtitles` set | Stock item documents expose addon subtitle tracks at both levels |

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


## 2026-09-18 stock 0.32.0 check
An isolated official 0.32.0 container used an online SQLite backup (integrity check passed) and loopback port 13001. Same user and Pursuit of Jade episode 4 on both servers: all seven production sources reported IsRemote=true, Protocol=Http, HTTPS paths. Stock reported IsRemote=false, Protocol=File, local paths, including both HLS variants. See UPSTREAM-0.32.0-CHECK.json (sanitized; no tokens/URLs). Production already uses the 0.32.0 image for assets with its patched binary bind mount. Do not confuse image version with binary compatibility. Stock has not passed retirement criteria. No physical client or seek acceptance was performed this run; retain patches. Test container removed after comparison.


## Vidhub external subtitle discovery: 2026-09-20

Reproduced on physical iPad, Vidhub 3.0.6, No Pain No Gain S1E12, with both Usenet HHWEB 2160p and TorBox/Torrentio 2160p versions. Before the Path change, the subtitle menu exposed only one embedded track; no external Vietnamese track. Fresh item and PlaybackInfo requests were observed. The item response under Vidhub's own session already contained the Vietnamese external track in both MediaStreams and MediaSources[].MediaStreams, and its DeliveryUrl returned HTTP 200 with valid WebVTT (86,913 bytes). No subtitle download request was observed during the baseline playback trace.

Adding Path to external addon subtitle descriptors made Vidhub list External Subtitle (1), download the subtitle and visibly render Vietnamese on both versions. Usenet also displayed Vietnamese after a forward seek to around 32 minutes. This is a remux Jellyfin metadata compatibility issue; vnphim's subtitle content and authentication were already working. Do not claim the earlier top-level mirroring alone fixed Vidhub.

The final candidate uses a language filename (e.g. vie.vtt) for Path and retains the authenticated DeliveryUrl for fetching. The first diagnostic candidate copied DeliveryUrl into Path, which proved discovery but caused Vidhub to show the URL/query as its label; do not retain that diagnostic version. Final language-filename build verified on the same physical iPad: Vidhub lists vie.vtt (no URL/query in the label) and visibly renders Vietnamese on both TorBox/Torrentio and Usenet HHWEB after reopening playback. All four episode versions also returned Path=vie.vtt and HTTP 200 valid WebVTT through their DeliveryUrl. Infuse was not physically retested in this run; its existing DeliveryUrl and subtitle contents were unchanged.

Retirement criterion: stock upstream exposes addon subtitles in the documents these clients read, with a usable Path/label and DeliveryUrl; actual Vidhub discovers and renders the track on explicitly selected debrid and Usenet versions, including seek/resume. Infuse should retain working subtitles. Test with a freshly opened item to avoid client metadata caching. Do not infer success from API fields or HTTP 200 alone. The patch affects subtitle metadata only, without changing video routes or adding a video relay. Precise subtitle synchronization across different encodes is a separate property, not guaranteed by this check.

Deployment: nimo LXC 111, 2026-09-20, binary SHA256 `3142908b1d1c95d83fa841de856ca9077f79712b1647a80ea81418d53f5cd608`. Built on seedbox in the existing Debian-trixie Rust container; `cargo fmt -p remux-server`, release build, `git diff --check`, live API checks and physical Vidhub playback passed. Original binary rollback: `/opt/remux/remux-server-patched.backup-vidhub-path-20260920T230927Z` inside LXC 111 (stop remux, stage/copy backup and recreate container; do not overwrite a running mapped binary). The original source snapshot is `/root/remux-patch/subtitles-before-vidhub-path.rs` on nimo. No vnphim change was needed. No new diagnostic logging remains enabled from this investigation.


## Episode release timing is separate from subtitle discovery (2026-09-20)
Follow-up on No Pain No Gain S1E12: Infuse 8.5.3 selected the Usenet HHWEB source, Vietnamese vie.vtt, subtitle Time Offset +0.00. Vidhub 3.0.6 on that same Usenet version also showed a mismatched cue (Shenhua International title while still in the apartment scene). The Path discovery fix works in both clients; rendering alone does not prove dialogue synchronization.

Compared original embedded subtitle timestamps using ffmpeg -copyts and -avoid_negative_ts disabled in short seek samples. Ordinary seeked SRT extraction can shift the output timestamps; do not use those relative samples as exact timing evidence.

| Dialogue anchor | Vietnamese external / TorBox embedded English | Usenet embedded Chinese | Usenet difference |
| --- | --- | --- | --- |
| Mr. Pei, don't worry | 00:10:59.190 | 00:11:14.261 | +15.071 s |
| To ensure users' needs are met | 00:33:37.710 | 00:34:10.700 | +32.990 s |
| 24-hour housekeeping | 00:33:43.950 | 00:34:16.940 | +32.990 s |

TorBox/Torrentio source 65e11276edf25e7296aa3315ccac006e matches the Vietnamese cue times exactly in both early and late samples. Usenet HHWEB source 8281a9552ee855b4b75f66d2a5db1a1f differs by a changing offset. Episode item: 8281a955-2ee8-55b4-b75f-66d2a5db1a1f. This is evidence of different release timelines, not a player-specific delay, and a single global offset is not a valid fix. We have not established all cut boundaries or guaranteed full-episode sync from two samples.

Practical workaround: select the matching TorBox release when using this Vietnamese subtitle. Long-term: match subtitle editions to the selected video source, or perform validated per-source alignment (including cuts) and cache that result by video/subtitle identity. The current same-episode fallback must not be described as synchronization-verified for every debrid/Usenet release. Do not add an episode-wide hard-coded delay or change the shared Vietnamese subtitle for all sources. No subtitle timing code or delay setting changed during this investigation.

During this follow-up LXC111 was found stopped. The user confirmed no maintenance and authorized restoring it; pct start 111 succeeded and remux health returned 200. The reason for the stop was not established.

## Optional embedded-text alignment (2026-09-20)

A separate opt-in feature now addresses the release-timing issue above. Remux resolves the selected source, extracts a full embedded text reference, and asks a private subtitle-only worker to propose and validate piecewise corrections. As of 2026-09-21, the worker uses ALASS only at the user's request; language-model validation is disabled. Only external English/Vietnamese missing as a full embedded language is eligible. Structural checks preserve text and valid timestamps but do not prove semantic correctness. See [the alignment operations guide](tools/subtitle-alignment/README.md) for limits, setup, acceptance criteria, validation evidence, and rollback.

This does not replace the remote-HLS or Vidhub discovery patches. Keep it as its own commit. Remove its configuration to disable it independently; retire the code only after upstream passes selected-release alignment, no-reference/rejection fallback, and source/cache isolation checks. Cold requests return the original while processing; a client must reload subtitles to receive a ready correction. Extracting an embedded track can read much of the video once, so the reference text is cached; video playback itself remains direct. Do not promise perfect alignment for every language or every release.

Initial 2026-09-20 alignment deployment used server SHA256 `ad335bb887a29154cf016cd7a0a13d4853443098191aa1696644e8a2700e636f` with worker `embedded-text-v2`.
Live SRT/VTT/Jellyfin JSON checks preserve all 1,070 E12 cues and verify the three
independent timing anchors; original bypass works. Rollback locations are recorded
in `/root/remux-patch/subtitle-alignment-rollback.txt` on nimo. Restoring its compose
backup disables alignment while retaining ordinary subtitle delivery.

Current ALASS-only behavior, measured latency, and rollback are documented in [ALASS-ONLY.md](tools/subtitle-alignment/ALASS-ONLY.md). Do not re-enable the model or claim historical semantic rejection tests apply to this mode.

## Bounded synchronous subtitle delivery — 2026-09-21

See tools/subtitle-alignment/SYNCHRONOUS-DELIVERY.md. Retire this part only when upstream serves a completed correction on a cold result-cache request (including concurrent requests), preserves originals on failure, and bounds waiting. Matching quality and subtitle delivery are separate acceptance checks. Long reference extraction can still exceed the request budget.

## Confirmed-dead stream fallback — 2026-09-22

Reproduced No Pain No Gain S1E18 in VidHub: the first/default Sootio 2160p source failed with a network playback error, while the next Torrentio 2160p version played in Remux Web. The source list still placed Sootio first. This points to one unavailable provider URL rather than an episode-wide playback failure.

The fork now preflights only the first three default HTTP source candidates with HEAD, stopping at the first source that is not confirmed missing. It removes a candidate from the default episode/movie source list only when the final response is HTTP 404 or 410, and caches that result for ten minutes. Other received HTTP statuses are cached for 30 seconds to avoid repeating HEAD checks when a client refetches the item; network/timeouts are not cached. It does not fetch media bytes. Timeouts, 401/403, 429 and 5xx remain visible. Explicitly requested stream groups bypass this filter, and if every checked candidate is missing the original source list is retained as a last resort. After the missing-result cache expires, a provider source can reappear automatically.

The filter runs before stream grouping and in Remux's central default PlaybackInfo selection, so clients that refetch PlaybackInfo do not silently select the same dead first source. Explicit source-ID requests still bypass the filter. This is a limited fallback check: it does not prove that a source returning HTTP 200 will decode or remain complete, and providers that mishandle HEAD keep their source visible. Initial deployment removed Sootio from Remux Web's default E18 list, leaving Torrentio first. A subsequent VidHub request still failed: its request log showed auto-play by episode ID, and Remux returned its empty-stream placeholder because lookup temporarily saw no eligible rows.

### VidHub refresh race — 2026-09-22

Root cause: `refresh_streams` wrote the episode's new `streams_refreshed_at` before upserting the refreshed child stream rows. During that brief interval, a concurrent playback lookup treated the episode as fresh, skipped refresh, and then `Media::streams()` excluded all old rows because their `updated_at` preceded the new timestamp; the replacement rows had not been inserted yet. That returned “no playable sources” even though the subsequent refresh contained Torrentio, Usenet, and both Vnphim entries.

Fix: write the fresh timestamp only after the refreshed child rows are upserted. The per-item refresh lock then keeps concurrent playback lookups from treating an incomplete refresh as complete. This fixes a Remux catalog race; it does not claim that every provider returning HTTP 200 is playable. Build, deploy, and repeat VidHub E18 playback before considering this resolved.

### E18 redirect-chain compatibility — 2026-09-22

Follow-up testing on the iPad reproduced VidHub's failure on the default Torrentio/TorBox E18 source; Infuse also showed a 404 during the same test window. PlaybackInfo used the authenticated `/remux/subtitle-ready/{item}/{source}/stream` route. That route returned a 307 to the Torrentio resolver, which then returned a redirect to the TorBox API and another redirect to the CDN (three redirects from the client before media bytes). A one-byte range request succeeded at the final CDN, so the provider file was reachable from the server; a HEAD-success/first-byte check did not establish that iOS clients could follow the redirect chain.

Commit `6640ec0e` changes the static playback route: for redirect-enabled external HTTP sources, it uses the existing safe HEAD resolution cache to redirect directly to the verified final public HTTP(S) target. If the check does not yield a safe target, the route falls back to the original add-on URL. It keeps the subtitle-ready check before video delivery and sends media bytes from the CDN directly to the client; no video body is proxied through Remux. The existing 30-second non-missing HEAD cache bounds signed target reuse; expired/missing cache entries trigger a new HEAD resolution. Internal hosts and HLS inline handling are unchanged.

Retirement check: run the focused redirect handler test, rebuild/deploy the fork, and on iPad test the first Torrentio/TorBox E18 source in VidHub and Infuse. Verify the route's `Location` goes directly to the CDN (not the Torrentio resolver), first-byte/range responses succeed, playback starts, and seeking works. This change is a redirect-chain reduction, not a deep check for missing middle file pieces; the current default source health filter still only confirms 404/410 with HEAD and cannot prove full-file completeness.



### Episode 18 iOS playback acceptance — 2026-09-22

After deploying commit `87ae4a9f` (live server SHA256 `59b24593ba9da4782b810b1aa1aef16231eb4623658fad0a0a64e5a33c529ba1`), verified the active Remux binary hash and healthy endpoint on Nimo LXC 111.

On the unlocked iPad, freshly opened No Pain No Gain S1E18 in VidHub and selected the first/default TorBox/Torrentio source. It started after the app's loading screen, showed Vietnamese subtitles, and continued playing through multiple scenes for over two minutes. The player overlay identified “No Pain No Gain Season 1 Episode 18 - Episode 18” and the source label “[TB⚡] Torrent…”. This is the first successful physical VidHub playback of the first TorBox/Torrentio E18 source after the failure.

The playback path change in `87ae4a9f` closes the subtitle-ready gate race for the first source: after Remux has verified readiness, PlaybackInfo advertises the validated final public CDN URL directly. It avoids making VidHub follow the Remux → Torrentio → TorBox API → CDN redirect chain. Later alternate sources retain the subtitle-ready route and readiness behavior. Video bytes remain CDN-to-client; this does not relay the movie through Nimo or the seedbox. The signed CDN target is runtime-only and must not be logged or saved in this note.

A scrub/seek attempt from the player overlay did not yield a reliable visible timestamp jump, so seek/resume is not accepted by this test. Infuse was brought forward during the same iPad session, but its existing mini-player state did not provide a clean, independently selected E18 test; do not count that as an Infuse acceptance. Repeat a deliberate mid-episode seek in VidHub and a fresh E18 start in Infuse before claiming full client-matrix success.



### VidHub second-episode smoke test — 2026-09-22

After the E18 test, VidHub resumed No Pain No Gain S1E1 Episode 16 from the home Resume Playback row (displayed progress 12:18 of 44:00). Playback continued through several scenes for more than a minute without an error. The source label disappeared with the player controls, so this is an episode-level smoke test only; it does not identify which provider/source played or prove full-episode completion. The test playback was stopped after observation.

### Background stream-prefill priority — 2026-09-23

During No Pain No Gain S1E18 playback, the live SQLite queue showed E18 at priority 100, E19 at 100, E20–E22 at 80, and recently played episodes at 40. E18/E19 stream lists had refreshed around 08:51 UTC and were still fresh at the 08:54 UTC inspection; their next refreshes were scheduled for about 09:04–05 UTC. The E19 tie came from the previous monotonic `MAX` upsert retaining a higher old priority.

The queue now assigns E19 (the next episode) priority 200, ahead of the current episode (100), following upcoming episodes (80), and recent history (40). Duplicate enqueue targets keep the highest priority within one playback event; a later playback event replaces stale priority values. Priority 200 jobs skip the normal 0–120 second refresh jitter. The change is deployed from commits `2d2177ce` and `bac64848`; focused tests passed (2/2), and the deployed binary SHA256 at the time was `e9e2e49ecc71a31ce13ff75015f05e8c04fe7521004b87b4b39667896799226d`. This update changes stream-list cache refresh order only. It does not pre-align subtitles or run full Usenet article scans in the background.

### Background probe of displayed source candidates — 2026-09-23

The background stream-refresh worker now probes every candidate row after refreshing stream metadata, with at most two probes running at once. It clears a clone's old `probe_data` before ffprobe so a stale result cannot count as verification. Movie and episode candidates must return a video stream; track candidates must return audio. Probe verdicts are kept in a process-local 15-minute cache keyed by the source ID and a hash of its full descriptor, so refreshed signed URLs do not inherit an earlier URL's status. The normal default PlaybackInfo path omits alternate candidates without a recent successful probe; explicit source-ID requests keep their existing probe path. Following a restart, alternatives stay hidden until background probing verifies them.

Deployed on Nimo LXC 111 with Remux binary SHA256 `635de30d29fb762b3429948f45a31e048e4d890fe39852bb3ad58b99ce6bc87a`; `/System/Info/Public` returned HTTP 200. The previous live binary is preserved at `/opt/remux/remux-server-patched.backup-probe-20260924T011657Z`.

For the runtime check, the pending No Pain No Gain S1E19 refresh was advanced to run immediately. Its four source rows were background-probed, and the job was rescheduled with zero retries. Authenticated default PlaybackInfo returned HTTP 200 with three entries (Torrentio and two Vnphim sources); the unusable Usenet/NZB candidate no longer appeared. This verifies Remux's source list behavior, not playback in a physical client. The probe establishes that ffprobe can read the expected media stream at check time; it does not prove the entire remote file will remain available or play through every client.

The isolated probe suite passed (30 tests), and the background-prepare test passed (1 test). The release build completed successfully. No video body or segments are relayed by this change.

### Love Hypothesis playback with subtitle alignment unavailable — 2026-09-24

VidHub's first three Love Hypothesis movie sources were reachable: one-byte range requests through Remux returned HTTP 206. PlaybackInfo initially failed because it synchronously waited for external subtitle readiness, and the video stream route repeated that wait; the subtitle worker had not produced a usable alignment. A PlaybackInfo request against the synchronous build timed out after 75 seconds.

Remux now schedules eligible English/Vietnamese subtitle alignment in deduplicated background tasks (two running at once) and returns PlaybackInfo/video without waiting for providers or alignment. Subtitle delivery remains fail-closed: at verification time the external subtitle route timed out before returning content, so no unaligned subtitle was served and Vietnamese subtitle availability remains unverified. Video delivery continues through the existing direct-to-origin route; Remux does not relay segments.

The same playback check exposed a restart edge case: source verification status is process-local, and the old filter treated “not verified since restart” as “unplayable,” reducing this movie to one advertised source. The default picker now hides only a recent explicit probe failure; unknown secondary sources remain selectable while background probing verifies them. The stream-group regression test covers this unknown-after-restart case.

Deployed to Nimo LXC 111 with Remux binary SHA256 `4cf94ede06aa25e04bbc18fd8267d07858e1aaca5af1396973c523be06042b3c`; health returned HTTP 200. The replaced binary backup is `/opt/remux/remux-server-patched.backup-20260924T100923Z`. A subsequent authenticated PlaybackInfo request returned HTTP 200 with all four sources in 2.4 seconds, and each of the first three source paths returned HTTP 206 for a one-byte range. Playback API tests passed serially (55/55), the probe-verification cache test passed (1/1), and subtitle-gate tests passed (3/3). No physical VidHub client playback test was available, and byte-range checks do not prove sustained playback or seeking.

### Subtitle-text delivery bounded to 2 s, MinResumeSeconds, explicit HLS pick survives a probe timeout — 2026-09-25

- `a7ecb8f4`: `gate::required`/`resolve_ready` removed. External subtitle requests call the alignment resolver once, bounded by `subtitle_alignment_wait_seconds` (default 10 s → 2 s); on timeout the already cue-validated original is served (HTTP 200, `X-Remux-Subtitle-Alignment: wait-timeout`) instead of a 503 after 60 s, and the spawned job keeps running for the next request. Verified on The Love Hypothesis: 503/60 s → 200 in 7.6 s cold, 2.04 s warm.
- `f6994139`: `ServerConfiguration.min_resume_seconds` (`MinResumeSeconds`, default 0 = off). When > 0 it replaces the `MinResumePct` rule so a fixed watch time (live: 60 s) creates the resume point regardless of runtime. `resume_verdict()` unit-tested. Caveat: the stock dashboard does not know the field and `System/Configuration` is a full replace — re-set it after any dashboard save.
- `c4bc90e1`: when the client named the stream and it is a remote HLS playlist, a probe error no longer fails PlaybackInfo: the filename guess is served and the failure memory is dropped without recording a verification. Cause: vnphim's VN-proxied kkphim/ophim dubs need ~21 s per ffprobe (segments cross the VN home uplink) against a 20 s budget; `ProbeTimeoutSecs` also raised 20 → 30 in the live config. Live binary `2ceb333c5e7cdb5b8ff8212af5f9cca4da1b8b1e3a0f2c82d3668ad0f5bb200e`.
- Retirement evidence for all three: stock serves a validated original subtitle within a few seconds when alignment is slow; stock exposes a fixed-seconds resume threshold (or the operator accepts percentage-only); stock lets an explicitly selected HLS stream through to the player when its probe exceeds the budget instead of returning 500 and hiding it.
- Related vnphim fix (own repo, `a33133e` on `kkphim-playlist-compat`): ad-strip now keeps one `#EXT-X-DISCONTINUITY` per removed ad cluster and injects `DISCONTINUITY-SEQUENCE:0` — the kkphim dub lost audio ~3 min in because the stripped playlist had an unmarked 25.6 s PTS hole where the first ad was.

### [+VN dub] sources via the seedbox dubmux muxer — 2026-09-25

`services/dubmux.rs` (+ config `dubmux_url`, `dubmux_public_url`, `dubmux_prepare_wait_secs`, `dubmux_prefetch_episodes`=20, `dubmux_prefetch_previous`=2, `dubmux_premux_next`, `dubmux_max_rows`=3; service sources in `tools/dubmux/`). For every vnphim Thuyết Minh/Lồng Tiếng stream × HQ HTTP release (≤ 6, list order = quality order) remux asks the seedbox muxer to prepare the pair; accepted pairs become synthetic stream rows cloned from the HQ source — "[+VN dub · <provider>] <name>", negative idx so they sort first, capped at 3 per item, probe rebuilt with the dub as default `vie` audio and every original audio track kept, embedded subtitles dropped (MPEG-TS), external addon/vnphim subtitles unchanged. The muxer serves a stream-copy HLS mux from `https://dubmux.geniallark.box.ca`; video bytes go CDN → seedbox → client, never through nimo or Cloudflare. `refresh_streams` never waits on a preparation; `StreamService::load` waits up to 12 s; a playback start walks the next 20 + previous 2 episodes once per series per hour and pre-muxes the next one.

Two alignment modes on the muxer: same cut → three-window FFT cross-correlation (accept when lags agree within 0.15 s, peak/rms ≥ 8; The Early Spring E02 hotphim: +0.021 s); different cut → piecewise (`align.py`): HQ audio fetched once, 60 s windows grouped into runs of constant lag, boundaries pinned to 0.5 s, one AAC track rendered on the video's clock with the release's own audio in the gaps (Pursuit of Jade E01 kkphim: −5.92 s until dub 178 s, −33.44 s after; ident, 27.5 s credits and tail from the original). Sign convention `video(t) ↔ dub(t + lag)` verified on a synthetic delay.

Verified live: Early Spring E02 lists two dub rows and plays in VidHub (user-confirmed by ear on 10:00 and 40:00 clips); Pursuit of Jade E01 lists dub rows after the piecewise path, clips at 2:40 (across the credits cut) and 20:00 rendered for the user's by-ear check. Deployed binaries `a9dac583…` (feature), `139229da…` (all HQ releases), `25ac570d…`/`014578b5…` (prefetch), then the row cap. Retirement: stock remux would need a way to attach an addon-supplied alternate audio track to another source's video, or to advertise an externally muxed HLS variant with its own probe; nothing upstream does this. Remove by unsetting `DUBMUX_URL`/`DUBMUX_PUBLIC_URL` (rows stop being created; existing rows age out with the 1-day stale-stream delete).

### dubmux follow-ups — 2026-09-25 (later)

- Row cap `dubmux_max_rows`=3 filled in HQ-quality order; prefetch on playback start walks the next 20 + previous 2 episodes once per series per hour (`dubmux_prefetch_episodes`/`_previous`) and pre-muxes the next one (`dubmux_premux_next`).
- `[+VN dub]` rows keep the HQ release's embedded subtitle streams (indices +100); subtitle delivery and the alignment reference extractor read them from the HQ file (`services::dubmux::hq_url_of`, the mux URL's `?video=`), so external Vietnamese tracks on those rows are auto-synced too (verified: Pursuit of Jade E01 row on the second 2160p release lists srt zho 103/104, embedded track fetches through the row, external vie reports `aligned`). Rows built from a release without text tracks list none.
- Background stream probing skips dub rows: ffprobe-ing a mux master starts a full-episode mux — one prefetch sweep produced 108 muxes / 298 GB before the fix. Muxer retention now 150 GB with a startup sweep and `POST /sweep`.
- Muxer routes answer HEAD (a 405 made remux's liveness check treat rows as dead, live-probe the mux and overwrite the synthesized probe — the reason embedded subtitles kept disappearing).
- VN-side extractor `tools/vnext` on VN LXC 100 (tailnet :8444): kkphim/ophim dubs extract with domestic bandwidth (~7 MB/s) and only the ~50 MB AAC crosses the VN uplink (Early Spring E02 kkphim: 111 s end-to-end vs ~10 min through MediaFlow); the seedbox muxer delegates via tailscaled's CONNECT proxy and falls back to the proxied fetch.
- Live binary `73bd7d3b183464c035250b468151ebde9ca4f81a32c6e26319fa7050aadbe4ae` (debug-log commits reverted).

### Pursuit of Jade E01 dub rows: gate route, alignment acceptance, embedded subtitles, `?ApiKey=` auth — 2026-09-25/26

User report in VidHub: three `[+VN dub]` rows, the first failed to start, the other two had the wrong audio from ~22:00, and none listed the HQ release's embedded subtitles. Four separate causes:

- **Gate route on mux rows (`cb71db25`):** the first row was built from a release WITH embedded text subtitles, so the subtitle-ready gate rewrote its `Path` to `/remux/subtitle-ready/{item}/{source}/stream`. That route serves an inline HLS body through remux, and the muxer master's relative `index.m3u8` then resolved against remux (404). Rows built from releases without text tracks were never gated and played. Mux rows are now exempt from the gate rewrite (their path is the muxer master itself; subtitles are still prepared in the background).
- **`?ApiKey=` with a token-less `MediaBrowser` header (`4b50b5f0`):** every VidHub request to the gate route in the last 24 h was 401 (4/4; my curl without the header was 200). The auth extractor returned early when an `Authorization: MediaBrowser Client=… DeviceId=…` header parsed, even with no `Token=`, and never looked at the query. Players send exactly that on media/subtitle URLs. Fixed like Jellyfin: keep the header's device metadata, take the token from `X-Emby-Token` or `?ApiKey=`/`api_key`/`token`. Unit test `token_outside_auth_header_reads_query_and_token_headers`. This affected every gated HQ source in VidHub, not only dub rows.
- **Alignment acceptance (`4b50b5f0`, muxer `align.py`):** `analyse()` judged coverage on run extent and stretched the last run to the end of the dub, so a pair with 3 confident 60 s windows out of 46 (kkphim dub × DDHDTV 2160p, whose audio simply does not match past the opening) was accepted as one constant −34.8 s offset — right for 3 minutes, wrong from ~22:00 (where a 15 s cut in the release shows). Coverage is now the fraction of coarse windows that are confident and inside an accepted run (`MIN_COVERAGE` 0.85), and a run only extends to the end when its last confident window is within two steps of it. The stored match records were bimodal (105 accepts ≥ 0.85, 37 accepts ≤ 0.5); the 38 low-coverage accepts and their aligned tracks / HLS sessions were purged, the muxer rebuilt, and their remux rows age out on the next stream refresh (a pair that is no longer `ready` is not re-upserted). Match JSON now carries `confident_windows`/`total_windows`/`covered_seconds`.
- **Embedded subtitles invisible (`cb71db25`):** dub-row tracks at `SUBTITLE_INDEX_OFFSET + n` were `IsExternal: false` with no `Path`; VidHub only lists tracks that look external and have a `Path`. `present_dub_row_embedded_subtitles` (PlaybackInfo and item documents) marks them external with `Path <lang>.vtt` and the authenticated `DeliveryUrl`; the stored probe keeps them embedded so the alignment reference detection is unchanged.

Retirement evidence: stock exposes an addon-muxed HLS row without routing its master through the server; stock authenticates `?ApiKey=` when the `MediaBrowser` header carries no token (Jellyfin behaviour); stock lists the muxed row's text tracks in VidHub/Infuse. Alignment acceptance lives in the muxer (`tools/dubmux/align.py`), not in remux.

Also `4f34ef23`: dub rows were numbered -1, -2, -3 in build order while sources sort by idx ascending, so the last-built (worst) release led the list; the first-built row now gets the most negative idx. Live binary `a65c1d16efb202f092c41fd3ceb2aef963e1840e41dfacf63028676a57519db2` (backup `/opt/remux/remux-server-patched.backup-20260925T232541Z`). Verified after deploy: VidHub-style request (token-less `MediaBrowser` header + `?ApiKey=`) on the gate route → 307 (was 401); Pursuit of Jade E01 lists three dub rows in order kkphim×PM 2160p, ophim×PM 2160p, kkphim×TB 1080p, each master answers HEAD 200 at the public muxer, embedded `zho`/`eng`/… tracks appear as external `<lang>.vtt` in PlaybackInfo and the item document and fetch through the row (642 cues); the DDHDTV pairs now reject (coverage 0.065, 5/46 confident windows). No physical VidHub playback re-test was done in this pass.
