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

The label is a last-observed result, keyed by exact media path, duration and sorted
subtitle descriptor, kept for 24h in the same bounded in-memory store. Different
releases/tracks do not share it; signed URL renewal may conservatively require
another request before the marker reappears. A provider replacing contents at an
identical URL is discovered on the next subtitle request. Restart clears labels.

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
