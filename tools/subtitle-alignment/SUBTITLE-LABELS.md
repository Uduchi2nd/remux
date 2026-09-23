# Auto-synced subtitle naming

When a delivered external subtitle has changed timestamps, Remux records that
fact and adds `[Auto-synced]` to its DisplayTitle and filename (Path) in subsequent
metadata responses. Example: `vie [Auto-synced].vtt`. Language codes and DeliveryUrl
remain unchanged. Addon tracks are labeled in item documents and PlaybackInfo;
sidecar tracks are labeled in PlaybackInfo. Vidhub uses the filename while other
clients may use DisplayTitle.

The worker explicitly compares every cue start/end with its original. Unchanged
text reserialization is not counted as a timing change. Such a successful no-op
returns `unchanged`; modified timing returns `aligned`. Rejection, timeout, missing
reference, same-language skip and disabled alignment do not mark a subtitle.
Original-bypass requests do not erase the normal delivery's last-known state.

The label follows the item, source ID, stable subtitle ID and language rather than
the signed provider URL or rewritten source path. Remux persists validated original
subtitle text and the successful per-source result on disk for seven days (up to
512 track records), so a renewed/broken provider URL or a Remux restart does not
discard a good version or its `[Auto-synced]` marker. Cache filenames are hashed;
signed URLs are not stored. The alignment result remains scoped to its media source
and exact subtitle contents.

Remux validates candidate external subtitle contents before advertising them. An
empty, malformed, HTML, or error payload with no timed cues is omitted from fresh
metadata; confirmed invalid responses are negatively cached for 15 minutes, while
timeouts and other transient provider failures are not treated as bad subtitles.
The subtitle-delivery route applies the same filter so hidden tracks do not shift
the advertised stream indexes.

Menus are normally fetched before the subtitle file. Remux cannot rename a menu
already held by a player; reopen playback or refresh metadata after the first
successful matching request. The marker means timing was adjusted, NOT audio sync
verified. Structural validation cannot establish translation quality or prove
alignment to dialogue. No reference-specific offset exception or language model is
enabled.

Worker version `embedded-text-v5-change-label` invalidates older results lacking
the timing_changed field. Remux treats a missing field conservatively as false.
Delivery records both the internal media path and the HTTP source URL, because
remote-source metadata exposes the latter. The same duration and exact subtitle
descriptor isolate both aliases from other releases. Live verification covers
this raw-source versus metadata identity boundary.

## Deployment verification — 2026-09-21

18 Rust subtitle tests and 16 worker tests passed. Live E12 Usenet delivery
changed both metadata names after the aligned response; language and subtitle
route were unchanged. Original bypass retained the label. E11 same-language
skip returned identical original bytes and remained unmarked.

Binary SHA256: `90fe42ede45f434de39ab3382e03acd39181f8ab8010788b6d831186dd3eae71`. Worker `embedded-text-v5-change-label`,
ALASS-only with no reference override. Full rollback binary
`/root/remux-patch/remux-pre-subtitle-labels` and worker
`/root/remux-patch/worker-pre-subtitle-labels.py`; restart both services.
The iPad was disconnected from USB, so no new physical client menu check is claimed.

```json
[
  {
    "before_title": "vie - WEBVTT - External",
    "before_path": "vie.vtt",
    "status": "aligned",
    "seconds": 0.284,
    "after_title": "vie - WEBVTT - External [Auto-synced]",
    "after_path": "vie [Auto-synced].vtt",
    "language_unchanged": true,
    "route_unchanged": true
  },
  {
    "source": "79df18dc781b5093a9144d500bd981db",
    "status": "embedded-language",
    "bytes": 71583,
    "original_unchanged": true
  },
  {
    "skipped_subtitle_unmarked": true
  }
]
```

## Verified deployment — 2026-09-23

Production binary SHA256: `9db6cdaab2d67c796ad720acee814735a56d82397d44f6055d11e80a1b0dbffa`.
On No Pain No Gain S01E18, the Torrentio and Usenet Vietnamese tracks each
returned HTTP 200, `aligned`, and 1,012 timed cues. Both displayed
`[Auto-synced]` in fresh PlaybackInfo. The two vnphim tracks returned HTTP 200,
`no-reference`, and 1,012 cues, so they remained unmarked. After restarting
Remux, fresh PlaybackInfo restored the aligned labels from the on-disk cache.
No empty E18 candidate was available as a live negative sample; empty and
malformed payloads are filtered by the shared structural validator before the
track is advertised. No physical player playback was tested.
