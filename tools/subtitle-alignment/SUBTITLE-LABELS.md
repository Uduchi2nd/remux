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

The label is a last-observed result kept for 24h in the bounded in-memory store.
It is keyed by item, source path, duration and the addon's stable subtitle ID, so
signed URL renewal keeps the marker attached to the same advertised track. An
exact descriptor key remains as a fallback for older callers. A provider replacing
subtitle contents while reusing its ID is discovered on the next subtitle request.
Restart clears labels.

Menus are normally fetched before the subtitle file. Remux cannot rename a menu
already held by a player; reopen playback or refresh metadata after the first
successful matching request. No new extraction or matching is triggered merely
to render a label. The marker means timing was adjusted, NOT audio sync verified.
No reference-specific offset exception or language model is enabled.

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
