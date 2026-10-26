# Paid media reads

Remux and the seedbox muxer share a persistent admission ledger for automated
media reads. These limits address the quota incident: embedded subtitle
extraction and dub preparation can read an entire release. Cached subtitle
text, completed muxes, and direct device-to-CDN playback do not need a lease.
No playback proxy or segment relay is added.

The initial policy is deliberately tighter for Premiumize:

| Policy | Premiumize | TorBox |
|---|---:|---:|
| Daily estimated reservation budget (decimal GB) | 10 | 80 |
| Concurrent admitted jobs | 1 | 3 |
| Minimum gap between full jobs | 3,600 seconds | 10 seconds |
| Minimum gap between background probes | 60 seconds | 5 seconds |

The full-job and probe start clocks are separate; they share the daily budget
and concurrency. Busy or over-budget jobs are deferred, not treated as broken
streams or rejected dub pairs. Direct foreground playback probes retain their
existing path; the background probe queue uses this ledger. The existing
placeholder circuit breaker remains an additional protection.

## Accounting and boundaries

This is admission control with **estimated reservations**, not an exact network
byte meter or a guaranteed cap on the provider account's bandwidth/points.
A full job reserves 110% of the known whole-file size. An indexed subtitle
job reserves its fixed 64 MiB body-read ceiling, sharing full-job start pacing.
It does not reserve the entire release size. A probe reserves the
smaller of the known file size and 128 MiB (128 MiB if size is unknown). A
full-file read without a valid size is denied. The extra 10% allows for normal
metadata and seek overhead; unusual demuxer reads can exceed an estimate.

Reservations stay spent after errors and timeouts. The day changes at midnight
UTC; counters survive both service restarts. Failed lease release can hold a
concurrency slot until its two-hour expiry, but does not refund the reservation.
The ledger retains 45 days of counters. It is not seeded from provider account
history, so the first day's reservations do not include earlier usage.

Remux's subtitle cache/in-flight extraction deduplicates clients into one pass.
The muxer's nested preparation, raw-copy and sync-check work for the same HQ
source shares its active reservation. A later independent read needs a new
reservation. A failed raw copy cannot fall through to a second remote full-audio
extraction or immediately retry the full download within that preparation.
Native HTTP subtitle extraction disables automatic FFmpeg reconnects.

Provider metadata identifies Premiumize/TorBox. Unknown HTTP sources use the
stricter Premiumize bucket; recognized local/Vietnamese/Usenet sources bypass
paid-provider accounting. The muxer binds trusted source metadata to URL hashes.
A TorBox-labelled source resolving outside TorBox's documented domains is
pinned to the strict bucket and deferred before its full read. Recognized
domains are `torbox.app` and `tb-cdn.{cx,io,pw,sh,st,to,earth}`, including their
subdomains; matching requires a DNS label boundary. The list comes from
[TorBox's official support page](https://support.torbox.app/en/articles/12667085-torbox-is-being-blocked-by-my-anti-virus-software).
Unknown destinations still use the strict policy. Existing strict pins are
preserved; correcting a false pin requires verifying the current destination
and backing up the affected registry entry. Classification corrections never
refund or reset previous reservations. No signed URL or provider API key is
stored in this registry or ledger.

## Configuration

The muxer reads these from its existing protected `~/dubmux/.env`:

```
DUBMUX_PREMIUMIZE_DAILY_GB=10
DUBMUX_PREMIUMIZE_CONCURRENCY=1
DUBMUX_PREMIUMIZE_START_GAP_S=3600
DUBMUX_PREMIUMIZE_PROBE_GAP_S=60
DUBMUX_TORBOX_DAILY_GB=80
DUBMUX_TORBOX_CONCURRENCY=3
DUBMUX_TORBOX_START_GAP_S=10
DUBMUX_TORBOX_PROBE_GAP_S=5
```

`DUBMUX_TRANSFER_TOKEN` in that protected file is a new shared authentication
secret. Generate it privately; never put it in Git, example commands, or logs.
Remux loads the same secret from a protected, read-only mounted file:

```
MEDIA_TRANSFER_BUDGET_URL=http://100.68.133.6:12500
MEDIA_TRANSFER_BUDGET_TOKEN_FILE=/run/secrets/media-transfer-budget-token
```

The budget service is independent of `DUBMUX_URL` and subtitle alignment URLs.
Missing/unreachable budget configuration denies new paid reads, while caches
remain available. These settings do not enable preparation or alignment.
Keep the quota-incident pause in place until separately resuming automation:
private `DUBMUX_URL` and alignment URLs unset, background refresh false, dub
prefetch counts zero, and premux-next false. The warmer's `PAUSED` file stays.

Set `DUBMUX_CACHE_URL` to the private mux service to keep already completed
VN-dub versions visible during that pause. This is independent of `DUBMUX_URL`:
item details and PlaybackInfo only GET small `/cached/<dub>/<hq>` metadata,
with no redirects, a two-second timeout and a 4 KiB response limit. The read-only
endpoint verifies the accepted/decode-checked match, completed local session,
playlist, nonempty segments and existing seekable-file layout. An old match or
layout alone is not enough. Missing/incomplete caches stay hidden. Lookups never
register a source, prepare a pair, create a layout, start a mux or read upstream
media. Existing row identities, tracks and checked-first ordering are retained.
This does not resume preparation, alignment, warming or next-episode prefetch.

Private Bearer-authenticated endpoints:

- `POST /transfer-budget/reserve`: `{provider, bytes, kind: "full"|"probe"}`;
  returns a lease UUID, or HTTP 429 with a reason and `Retry-After`.
- `POST /transfer-budget/release/<lease>`: releases concurrency only.
- `GET /transfer-budget/status`: aggregate estimated reservations and policy.

Uncached `/prepare` now requires the same Bearer token, plus `video.provider`
and positive integer `video.size`. Cached preparation results remain readable.
Cold legacy mux URLs without registered source metadata cannot start a new
paid read. A freshly authenticated Remux request registers the source; old
subtitle-less muxes are never automatically re-downloaded for backfill.
The separate warmer accepts `DUBMUX_TRANSFER_TOKEN` in its environment or a
mode-600 `media-transfer-budget-token` file beside `warm.py`.

The ledger is `/data/provider-budget.sqlite`; trusted source metadata is in
`/data/provider-sources`. Keep these when recreating the muxer. Inspect status
with the secret loaded privately into a request header; never dump `.env`,
container environment, request headers or signed URLs into diagnostic output.

## Verification and rollback

Run `python -m unittest discover -s tools/dubmux -p 'test_*.py'`. Run
`verify_transfer_api.py` inside the candidate muxer image with `--network=none`;
it uses temporary data, tiny synthetic budgets and loopback HTTP, including a
restart check. Focused Remux tests cover budget authentication/release,
classification and existing subtitle/cache behavior. Production verification
uses health checks and existing small subtitle caches, never a paid video read.

Before deployment back up changed host sources, the protected environment,
compose configuration and the previous image/binary. Deploy the muxer first,
then the Remux secret mount and binary using the existing stop/swap deployment
helper. Preserve the data directory and all automatic-work pauses. Rolling
back an image/binary must not remove the ledger or resume automation.


## Reduce extraction traffic

Paid remote **text** subtitle cache misses use authenticated `POST
/subtitles/indexed` on the same private service configured by
`MEDIA_TRANSFER_BUDGET_URL`. The request has `video: {id,url,provider,size}` and
`subtitle_ordinals` (the original `0:s:N` ordinals). The service reads bounded
HTTP ranges using Matroska SeekHead/Cues and returns a small Matroska artifact
containing the original metadata and selected native subtitle packets. Remux
runs its existing FFmpeg conversion on that local artifact, retaining native
ASS and plain text in their separate caches. Original timestamps and the
existing quality checks remain. Concurrent requests share one extraction per
requested track; other languages do not join a job that omits their packets.

Limits per job: 64 MiB of response body data read (including overfetch), 6,000
HTTP requests including redirects, four connections, 20 request starts/second,
300-second deadline and 16 MiB artifact. Redirect bodies are closed without
being drained. A server ignoring Range is rejected before its body is read;
changed size/ETag, invalid offsets, absent/inadequate indexes, unsupported text
codecs and limits cause deferral, never a sequential paid-video fallback.
Stats in `X-Remux-Extraction-Bytes/Requests` describe this reader's response
bodies, not TCP/TLS/header overhead or other provider account activity.
`GET /transfer-budget/status` reports the indexed limits too.
Indexed source registration is separate from full mux registration, so a text
request cannot make an old unfinished mux eligible for automatic full reads.

The index is advisory: Matroska recommends indexing every subtitle frame but
does not guarantee it. Missing position/duration information is refused, and
Remux's normal quality checks reject stubs/invalid results; these checks cannot
prove that an otherwise well-formed index lists every subtitle in a release.
PGS/image subtitles and recognized unpaid sources retain their existing opt-in
extraction path; the bounded indexed path is for paid text subtitles. No new
extraction permission is granted, and all automatic-work pauses stay in place.

Dub matching uses a completed raw cache's local audio and local duration probe,
without resolving/reading the HQ URL or reserving another paid transfer. HLS
dub extraction selects a separate Vietnamese/default audio rendition (or an
audio-only variant) when available, preserving the original master identity for
cache reuse. Group selection, quoted attributes, initialization fragments and
byte ranges are respected. An in-band default is not replaced with an unrelated
language merely to avoid video. The selected audio URL is also passed to the VN
extractor; retries keep that URL. Mixed audio/video sources still require their
mixed segments when no usable audio-only rendition exists.

Verify with synthetic media and `--network=none` candidate containers. The
indexed fixture compares SRT and native ASS output against a full local scan;
`verify_cached_audio.py` checks the cached audio clock and asserts no paid URL
resolution/reservation during matching. Production smoke tests read only health,
aggregate policy and pre-existing subtitle caches.
