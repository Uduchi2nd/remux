# Subtitle readiness playback gate

Enable with Config.subtitle_alignment_gate_base_url (environment SUBTITLE_ALIGNMENT_GATE_BASE_URL), set to the public Remux URL. Default None preserves ungated behavior. Private alignment worker URL and token must also remain configured.

## Scope and behavior
For external English/Vietnamese tracks whose language is absent from full embedded tracks, and whose source has an eligible embedded text reference:
- PlaybackInfo prepares the first/selected resolved source before returning playable metadata.
- Eligible source Path in item documents and PlaybackInfo points to Remux's authenticated `/remux/subtitle-ready/.../stream` endpoint instead of the provider URL. This covers clients that skip PlaybackInfo.
- The video endpoint awaits preparation before any video bytes or provider redirect.
- Eligible subtitle requests also wait for alignment. They no longer return the original subtitle on the ordinary10-second timeout, preventing concurrent clients from loading a fallback while video waits.
- Preparation includes external addon subtitles and torrent sidecars. The same language filter and ten-per-language menu limit are applied before preparing addon tracks; all matching advertised candidates must prepare successfully.
- Both changed and unchanged successful alignment allow playback. Only changed timestamps receive [Auto-synced].

The overall preparation/request limit is60seconds. Pending alignment joins the existing job. On failure, requests return an error, not video or fallback subtitle text. Video returns503 with Retry-After and no-store; PlaybackInfo returns502 with a subtitle-readiness explanation. Long extraction jobs may finish in the background, permitting a later retry. No infinite spinner guarantee is possible: clients may impose shorter timeouts or show their own generic error UI.

Sources with no eligible embedded text reference, no applicable external target subtitle, or a full embedded track already in the target language remain outside this alignment gate. This is not a global requirement that every movie have subtitles, and does not verify the reference against audio.

## Upstream response validation
External subtitle fetching now checks HTTP success and rejects empty/error payloads without actual timed dialogue cues. This prevents an upstream JSON error such as #75 Bad Request from becoming blank WEBVTT. Existing SRT/VTT and ASS dialogue payloads are recognized. Valid raw text is cached for30minutes, keyed by the exact subtitle descriptor, so concurrent preparation/delivery uses the same input. Alignment results remain guarded by release identity and exact subtitle contents. No video is cached by this change.

## Direct bandwidth and old clients
Once ready, existing redirect behavior is preserved: the video/CDN still serves media directly where configured. A client already holding an old provider URL can bypass Remux entirely; refresh/reopen metadata to obtain the gated URL. The server cannot revoke already-issued third-party URLs or control a subtitle/video file already loaded in a player.

## Tests and deployment
Unit coverage: error/empty subtitle rejection, valid SRT/VTT recognition, reference/target-language eligibility, plus existing pending wait, source/content isolation, naming, and redirect regression tests. Live concurrent subtitle/video and error-block checks are recorded after deployment.

Rollback is removing SUBTITLE_ALIGNMENT_GATE_BASE_URL and recreating Remux, or restoring the pre-gate binary and compose backup. The gate is independently removable from HLS, naming, and subtitle-discovery fixes. E12-specific reference offsets remain removed; production alignment remains ALASS-only.
