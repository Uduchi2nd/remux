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
