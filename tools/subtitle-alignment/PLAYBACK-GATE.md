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

The overall preparation/request limit is 60 seconds. Pending alignment joins the existing job. On failure, requests return an error, not video or fallback subtitle text. Video returns 503 with Retry-After and no-store; PlaybackInfo returns 502 with a subtitle-readiness explanation. Long extraction jobs may finish in the background, permitting a later retry. No infinite spinner guarantee is possible: clients may impose shorter timeouts or show their own generic error UI.

Sources with no eligible embedded text reference, no applicable external target subtitle, or a full embedded track already in the target language remain outside this alignment gate. This is not a global requirement that every movie have subtitles, and does not verify the reference against audio.

## Upstream response validation
External subtitle fetching now checks HTTP success and rejects empty/error payloads without actual timed dialogue cues. This prevents an upstream JSON error such as #75 Bad Request from becoming blank WEBVTT. Existing SRT/VTT and ASS dialogue payloads are recognized. Valid raw text is cached for 30 minutes, keyed by the exact subtitle descriptor, so concurrent preparation/delivery uses the same input. Alignment results remain guarded by release identity and exact subtitle contents. No video is cached by this change.

## Direct bandwidth and old clients
Once ready, existing redirect behavior is preserved: the video/CDN still serves media directly where configured. A client already holding an old provider URL can bypass Remux entirely; refresh/reopen metadata to obtain the gated URL. The server cannot revoke already-issued third-party URLs or control a subtitle/video file already loaded in a player.

## Tests and deployment
Unit coverage: error/empty subtitle rejection, valid SRT/VTT recognition, reference/target-language eligibility, plus existing pending wait, source/content isolation, naming, and redirect regression tests. Live concurrent subtitle/video and error-block checks are recorded after deployment.

Rollback is removing SUBTITLE_ALIGNMENT_GATE_BASE_URL and recreating Remux, or restoring the pre-gate binary and compose backup. The gate is independently removable from HLS, naming, and subtitle-discovery fixes. E12-specific reference offsets remain removed; production alignment remains ALASS-only.


### Verified deployment (2026-09-21)
- Production binary SHA256: `08cfabab7868c09b8ead84b61d4c1082f32958af58efcc29d025d2c8981a50cc` (production implementation commit `7592f014`).
- 20 subtitle tests and two redirect regression tests passed before deployment. Three gate tests passed afterward, including an isolated unavailable-worker test asserting HTTP 503 and absence of original subtitle text. This added regression test does not change production code.
- E15 TorBox's upstream subtitle recovered: 819 Vietnamese cues, `aligned`, and `vie [Auto-synced].vtt`. PlaybackInfo advertised a gated source and an auto-synced track. The original expectation of a live E15 failure was therefore not a valid negative test; failure behavior was checked in isolation instead.
- E13 concurrent subtitle/video requests returned aligned text and an origin redirect. Warm E13/E15 requests took 0.02–0.18 seconds; these are not cold extraction benchmarks.
- Public gated route returned HTTP 307 through the reverse proxy with a browser user agent. Video redirects were not followed during this check. No physical iPad playback verification was performed.
- Backups on nimo: `/root/remux-patch/remux-pre-playback-gate` and `/root/remux-patch/compose-pre-playback-gate.yml`.

The gate guarantees server-side preparation before a new gated playback response, not that a client has selected/rendered the track, or that the reference itself matches audio. Refresh client metadata to replace previously cached direct provider URLs.


## Infuse constructed subtitle URLs after source fallback (2026-09-22)
Observed an E18 PlaybackInfo probe fail on source 92b139c6 and fall back to TorBox 7a16655f. Infuse constructed its subtitle URL using the advertised episode alias, rather than the explicit DeliveryUrl. The episode-alias route for subtitle index 4 returned 404 `subtitle stream not found`; the actual TorBox-source route returned 200 with 1,012 aligned Vietnamese cues. Video first-byte requests through the readiness gate and provider redirects succeeded. E16/E17 direct first-byte and subtitle DeliveryUrl checks had also succeeded, so these alone did not verify client behavior.

Fix: PlaybackInfo records the source actually probed for each advertised and delivery source ID, scoped by authenticated device and item, with a six-hour TTL. Subtitle endpoints resolve this mapping before selecting embedded/addon tracks or alignment input. Sidecar route tables remain keyed by the originally advertised identity. Subsequent PlaybackInfo replaces the mapping, avoiding stale fallback selection when switching sources. This does not alter the subtitle gate or proxy video bytes. Clients must request PlaybackInfo again after deployment to populate the mapping; old sessions and genuinely unavailable upstream subtitles are not repaired by this mapping.

Regression coverage verifies alias resolution, device/item isolation, and replacement on subsequent selection. Live verification is recorded below after deployment.
