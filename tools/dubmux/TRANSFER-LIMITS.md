# Paid media reads

Remux and the seedbox muxer share a persistent admission ledger for automated
media reads. These limits address the quota incident: embedded subtitle
extraction and dub preparation can read an entire release. Cached subtitle
text, completed muxes, and direct device-to-CDN playback do not need a lease.
No playback proxy or segment relay is added.

The initial policy is deliberately tighter for Premiumize:

| Policy | Premiumize | TorBox |
|---|---:|---:|
| Daily estimated reservation budget (decimal GB) | 10 | 200 |
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
A full job reserves 110% of the known whole-file size. A probe reserves the
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
A TorBox-labelled source resolving outside `*.torbox.app` is pinned to the
strict bucket and deferred before its full read. This is conservative for
unrecognized TorBox CDN domains. No signed URL or provider API key is stored
in this registry or ledger.

## Configuration

The muxer reads these from its existing protected `~/dubmux/.env`:

```
DUBMUX_PREMIUMIZE_DAILY_GB=10
DUBMUX_PREMIUMIZE_CONCURRENCY=1
DUBMUX_PREMIUMIZE_START_GAP_S=3600
DUBMUX_PREMIUMIZE_PROBE_GAP_S=60
DUBMUX_TORBOX_DAILY_GB=200
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
