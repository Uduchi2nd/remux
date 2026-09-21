# ALASS-only production mode — 2026-09-21

User requested the fast timing path without multilingual-model validation. Production Engine no longer constructs Encoder, loads ONNX/tokenizer, or calls semantic/exact-language validation. Those helpers remain available only for historical offline comparisons. Model files may remain installed but are unused. Version and result-cache namespace changed so old rejected/accepted results cannot hide the new behavior.

## Eligibility in remux

Only external English/Vietnamese subtitles are processed, with language aliases normalized by remux's existing helper. If the selected stream has a non-external, non-forced embedded subtitle in the requested language, skip before reference extraction, worker dispatch, and cached corrections. This includes bitmap tracks because the requested language is already present. Forced/signs-only tracks do not substitute for full dialogue. Other/unknown external languages skip. Return original bytes and `language-skipped` or `embedded-language`; do not hide tracks or change the player's subtitle selection. Other-language full embedded text remains the timing reference. Missing reference remains original.

## What remains checked

Unchanged cue count/text, valid nonnegative times, positive cue durations and chronological starts. ALASS has a 20-second subprocess deadline; failure preserves the original. Reference extraction remains separately bounded and cached. No speech recognition, translation, audio scan, or video transcode was added.

These are integrity checks, NOT proof of correct synchronization. Without semantic validation a wrong episode/reference can be accepted. This is a deliberate user-requested speed tradeoff; do not describe earlier semantic rejection tests as current safeguards. Source-specific cache isolation, originals, resource limits and private authentication remain.

## Verification

15 worker regression tests, including a mocked Encoder that raises if production constructs it. Real engine tests confirm ONNX was never imported. Original No Pain No Gain E12: 1.31 seconds uncached, all 1,070 cues preserved, previous independent anchors 8/10/10 ms from reference. Real E11 Vietnamese against embedded Vietnamese took 1.07 seconds in the low-level worker test; production now skips that same-language situation. Spanish controls likewise exercise the worker offline, but remux will not dispatch Spanish.

Sixteen timing controls across No Pain No Gain E11, Chad Powers E4, Supergirl and Die My Love retain all cue text. ALASS-only times and full results are in `alass-only-results-2026-09-21.json`. Fixed offsets/cuts recover within 0–222 ms depending on title; 4% artificial rate drift has worst errors up to 1,366 ms. These compare known subtitle timelines, not new audio/lip-sync measurements. Production behavior still depends on the selected release and supplied subtitle being the correct title/episode.

The first request still returns original subtitles while extraction/alignment runs; reselect after completion. Removing inference does not remove remote embedded-track extraction cost. No prefetch or video downloads to disk were added.

Keep this optional feature independently removable from the remote-HLS/Vidhub compatibility patches. Remove alignment URL/token configuration and recreate remux to disable it. Rollback instructions and deployed hashes are recorded after live verification.

Live worker validation: original E12 pair processed uncached in 1.23 seconds. `/proc` confirms no ONNX runtime mapped in the worker; systemd memory current approximately 25 MB. This is the deployed service, not just an offline benchmark.

## Deployed and verified

Production remux SHA256 `db358ae4227324ad688dd09eedb7b2e02e220f1a759b419d9ecb6eff15c89fd0`; worker SHA256 `26054fce79036a7f0194718d66e32413ccc9b9068d1e26e5d510096eb7f1146c`. Thirteen targeted Rust tests and 15 worker tests passed. Live E11 source containing embedded Vietnamese returned `embedded-language` and byte-identical original subtitles. E12 missing Vietnamese returned the corrected result in SRT/VTT/Jellyfin JSON, all 1,070 cue texts preserved, original bypass works, three reference anchors 8/10/10 ms. Remux and worker are healthy.

Rollback assets: binary `/root/remux-patch/remux-pre-alass-only-v4` on nimo (outside the full LXC), worker `/opt/remux-subtitle-alignment/worker.py.pre-v4-20260921`. Stop remux, restore binary into LXC111 `/opt/remux/remux-server-patched` with mode 755, restore worker, restart its service, and recreate remux with the existing compose file. This restores v3 semantic behavior; disabling alignment entirely is a separate option using the configuration described in README.

## E12 Vidhub follow-up — 2026-09-21 UTC

Report: the Usenet E12 Vietnamese subtitle was out of sync around 07:18. Examined the complete preserved external/reference/aligned files and a real Vidhub session. No algorithm or deployment change was made for this investigation; production remains v4 ALASS-only with the model disabled.

- ALASS adds 15.079 seconds to the first 389 cues and 33.000 seconds from cue 389 (original start 964.140 seconds). All 1,070 texts remain unchanged. These are two timing sections, not a guarantee of full-episode semantic accuracy.
- At 07:18, corrected Vietnamese cue 145 spans 437.670–439.699 seconds; its corresponding embedded Chinese cue spans 438.061–439.700. The corrected start is 391 ms early; the end differs by 1 ms. Original Vietnamese starts at 422.591 seconds, about 15.47 seconds before the reference.
- Before reopening Vidhub, a visible cue appeared to match the original timeline near 11:43, but screenshot/check-in latency prevents treating that as definitive proof of the previous file.
- Closed and reopened Vidhub. A sanitized server trace captured its external subtitle GET followed by X-Remux-Subtitle-Alignment: aligned. No credentials or signed URLs were retained in the trace.
- Paused fresh playback at server-reported 1145.989 seconds. The visible Vietnamese cue is at 1145.580–1146.300 in the corrected file and 1112.580–1113.300 in the original. Embedded reference: 1145.579–1146.299. This confirms the fresh player was displaying corrected timing at that point, within 1 ms of the embedded reference. This is a text-timeline check, not a measured audio/lip-sync test.
- The iPad locked before a fresh visual seek to 07:18 could be completed. Do not claim that visual check or whole-episode audio verification passed.

### Known first-request delivery limitation

`alignment::resolve` returns original bytes with `pending` while its background job runs. An already loaded subtitle is not replaced in the player when that job completes. Even private/no-store HTTP headers cannot replace an in-memory player track. A fresh subtitle request after completion returns the corrected file; reopening Vidhub was verified to do this. This remains unresolved in code and can look like ALASS failed although the corrected file is ready.

Do not re-enable the language model to fix this delivery issue. A follow-up should evaluate a bounded wait for fast/cached-reference alignment and a client-compatible way to refresh/version subtitle delivery, without introducing long startup stalls or mixing different release timelines. Full cold reference extraction can still be slow, so a short wait alone does not solve every first-play case.

### Unlocked-iPad follow-up

Resumed Vidhub and sought into the reported scene (visible timeline 07:15 then 07:28). Paused at server-reported 466.2705168 seconds (07:46.27), IsPaused=true. The displayed Vietnamese cue matches corrected cue 161, 465.189–466.803 seconds; original is 450.110–451.724. Corresponding embedded Chinese cue starts at 465.181 and ends at 466.500, with the sentence continuing in the next cue. This independently confirms corrected subtitle delivery in the nearby scene: start difference 8 ms, translated cue end 303 ms later. The device locked again while paused before exact 07:18 measurement. No audio was captured, so do not claim audio/lip-sync validation or exact 07:18 visual verification. Production code and model-disabled policy remain unchanged.

### Live worker multi-sample checks — 2026-09-21

Twelve authenticated HTTP requests to the deployed v4 worker: four pairs, each original, artificial +19-second offset, and repeated offset request. E11 Vietnamese/Chinese: 0.957–0.983s uncached, 0ms recovery error; Chad Powers E4 English/Spanish: 0.307s, 85ms; Supergirl English/Spanish: 1.977–2.069s, 204ms; Die My Love English/Spanish: 1.205–1.206s, 38ms. Errors compare output cue starts with the unshifted input sample timeline, not independently measured audio truth. All cue text/counts preserved; repeats 2–3ms. Cache absence/presence checked before each request. Results: live-multisample-2026-09-21.json. Fixtures were sent directly to worker; this does not bypass production eligibility or claim these titles were tested in players. Embedded-reference extraction time excluded.

Live Remux recheck: E12 external Vietnamese returns aligned; original bypass works. E11 selected source with embedded Vietnamese returns embedded-language and byte-identical original. Deployed binary and worker hashes still match those above. Remux cold delivery is STILL asynchronous (original/pending); worker HTTP computation is synchronous. No synchronous-delivery fix deployed during these checks.
