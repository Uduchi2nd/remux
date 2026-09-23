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

Remux advertises an external subtitle only after it has fetched and validated a
copy with timed dialogue cues, or can load a previously validated copy retained for
seven days. Empty, malformed, HTML, error, unavailable, or not-yet-verified tracks
are omitted from metadata. This fails closed when a provider times out or returns
a transient error; it does not classify that response as a bad subtitle. Confirmed
invalid responses are negatively cached for 15 minutes. The subtitle-delivery
route applies the same filter so hidden tracks do not shift the advertised indexes.

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

Production binary SHA256: `22ddbcb8367931d071a21c5953fe4c12c7dc63662c0b05926a0c21532f79968d` (build commit `5e644c9f`; canonical commit `6d1c75e7`).
On No Pain No Gain S01E18, fresh PlaybackInfo returned HTTP 200 after restarting
Remux. The Torrentio and Usenet Vietnamese tracks remained `[Auto-synced]` from
the seven-day on-disk cache. All four advertised external Vietnamese subtitle
routes returned HTTP 200 with 1,012 timed cues; the vnphim tracks are valid but
have no eligible alignment reference, so they remain unmarked. Empty, malformed,
HTML, error, unavailable, and not-yet-verified tracks are now omitted from
metadata. No empty E18 provider candidate was available as a live negative
sample. No physical player playback was tested.
