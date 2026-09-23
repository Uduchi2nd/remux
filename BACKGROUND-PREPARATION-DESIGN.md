# Background preparation, freshness and cache design

Status: implementation in progress, reviewed 2026-09-23. Code changes are local and not deployed.

## Goal

Playback requests should mostly read prepared metadata. Remux should prepare stream listings, redirect targets, Usenet availability signals and eligible aligned subtitles after a user starts an episode, while also preparing the next episodes and refreshing episodes played in the last seven days. Video bytes should continue to flow directly from the selected provider to the player; preparation must not turn Remux into a media relay.

Freshness targets:

| Result | Fresh (serve immediately) | Refresh target | Retention / fallback |
| --- | ---: | ---: | --- |
| Addon stream candidates | <= 15 minutes | Refresh warm entries before 15-minute expiry, with jitter and provider-wide spacing | Keep last-known-good candidates on transient refresh failure; do not treat that as proof a URL still works |
| Redirect resolution | Until the earlier of 15 minutes or known URL expiry | Refresh warm entries before expiry only if useful; resolve again immediately on expiry/failure | Short-lived and scoped to exact source identity; never trust a stale signed URL after 401/403/expiry |
| Usenet file readiness | <= 15 minutes | Refresh warm entries before 15-minute expiry, with provider-wide rate limits | Cache per NZB/file/backend and probe coverage; unknown is not complete; retain last result but mark stale |
| Accepted aligned subtitle | Serve cached result immediately | Recompute only when inputs/algorithm change or result expires | Keep successful non-empty correction for 7 days; reject empty output and retain original subtitle |
| Embedded subtitle reference | Serve cached reference immediately | Re-extract when source identity changes or reference expires | 7 days, bounded disk usage as today |

The 15-minute value is both the target freshness window and the maximum normal refresh interval for warm stream-list and Usenet-readiness entries. It is not permission to poll every episode in a burst. The scheduler spreads refreshes across that window and starts before expiry, so a successful refresh normally keeps a warm entry fresh. If an entry becomes stale first, return safe last-known-good data immediately and coalesce a refresh; do not wait for the background queue. Short-lived URL signatures and confirmed failures override this interval. Never convert a transient refresh error into an empty stream list or overwrite a good subtitle with an empty/failed result.

## Work ownership

| Work | Owner | Reason |
| --- | --- | --- |
| User/watch history, next-episode selection, durable queue, priorities and cache policy | Remux | It knows the authenticated user, selected item/source, Jellyfin episode order and playback lifecycle |
| Provider-specific URL discovery, domain rotation, stream construction and provider cache | VNPHIM / addon | Provider behavior and domains belong with the addon that understands them |
| Redirect resolution, source preflight, Usenet file probe orchestration, subtitle selection/alignment and Jellyfin delivery | Remux | These decisions depend on the selected source and the Jellyfin client contract |
| NNTP article-existence API and NZB job health | UsenetStreamer / NzbDAV | These services already speak NZB and provider protocols and can do checks near the backend |
| ALASS execution | Initially existing isolated worker; later optional Remux-managed worker | Keep native/Python dependencies and CPU/memory failures away from the HTTP/API process |

## Events and episode scope

Record a small, privacy-conscious playback activity row when playback starts and periodically while it advances. On a start event:

1. Playback itself always wins and is not held for background work. Enqueue the next episode at the highest background priority as soon as playback starts.
2. Resolve the next one to three *currently available* episodes in the same series using Jellyfin season/episode order and enqueue their listings and low-cost checks. The first next episode outranks the current-episode refresh, following episodes, and history refreshes.
3. Refresh work for every episode the user played in the last seven days. Collapse duplicate requests across devices/users when the underlying data is user-independent; keep user-specific authorization and watch history separate.
4. Do not crawl an entire library, prefetch whole video files, or queue work for every alternate source without a limit.

The active request always wins. A practical queue order is: (1) work blocking a current playback request, (2) the next episode's listing and cheap checks, (3) other upcoming episodes, (4) current-episode background refresh, (5) recently played items, (6) expensive subtitle reference extraction. Set global and per-provider concurrency caps, a bounded queue, and per-item deduplication. On overload, discard/re-coalesce low-priority refresh work rather than delay playback.

### Activity-based background cadence

Background work should be scheduled per series or movie, not by assigning a repeating timer to every episode. Use a shared provider budget/token bucket and spread work over time with jitter so a playback event or service restart cannot trigger a burst. For warm stream-list and Usenet cache entries, schedule refresh around 12–14 minutes after success, so the 15-minute cache remains fresh under normal latency. If a provider is busy, defer low-priority work and serve last-known-good data rather than exceeding its limit.

- **Currently playing episode:** prepare now; refresh warm stream/readiness entries around 12–14 minutes after success, or sooner if a foreground request shows the source expired or dead. Coalesce all duplicate player/client requests.
- **Next one to three available episodes of a series:** prepare listings and cheap readiness checks after the current episode begins. While the series is active and budget allows, refresh each warm entry before its 15-minute cache expiry. If load is too high, prioritize the next episode and defer lower-priority work.
- **Other episodes in the same series:** retain their cache records, but do not keep polling them. When any episode of that series is played, prioritize the next episodes and refresh recently played episodes in that series as budget permits. Past episodes with no recent activity can be rechecked on demand.
- **Episodes watched in the last seven days:** keep their cache entries available. While the series is active, refresh eligible stream/readiness entries on their own 12–14-minute schedule, with at most one queued task per episode/source and per-provider rate caps. If no episode in that series has been played for 24 hours, stop periodic refresh; retain cached data for the 15-minute freshness window, then refresh on the next play/request.
- **Movies:** prepare on play and keep the movie warm for seven days. While the movie is actively being watched, refresh only as needed (stale playback request or expiring source). After playback, do not poll it periodically; let the next request refresh it.
- **Aligned subtitle files:** a successful correction is reusable for seven days regardless of whether the show is still active. Do not recompute it on a timer. Recompute only after a source/subtitle/reference/algorithm fingerprint changes or the seven-day result expires, ideally before a playback request if the title is active.

The 12–14-minute schedule is an initial target, not a guarantee that every provider can meet it. If observed provider limits or queue load make it unsafe, reduce the number of warm episodes or lengthen the shared freshness window instead of hammering providers. Apply a global per-provider minimum interval and exponential backoff after 429/5xx/timeouts. Keep redirect TTL no longer than the signed URL lifetime; do not refresh aligned subtitles on this cadence.

## Cache identity and stored data

Use a durable SQLite-backed cache/job table in Remux's existing data area, with schema versioning. Do not rely only on process memory: today the stream HEAD cache and aligned-result cache are process-local, so a restart loses useful warm state. A row should carry `kind`, stable `work_key`, input/version fingerprint, state, `verified_at`, `expires_at`, last error class, and compact safe result metadata. Suggested states: `queued`, `running` (with lease expiry), `succeeded`, `rejected_empty`, `failed`, and `stale`.

Use stable identities rather than ephemeral request URLs:

- Candidate listing: user-independent item/provider/query identity plus addon/config generation.
- Redirect: stable source/release ID and original URL identity; store hop status, final safe host and check time. Do not persist full signed query strings or credentials. Re-resolve if provider signatures expire or return authorization/not-found errors.
- Usenet readiness: backend/provider, NZB GUID or stable release identity, selected file ID, file length and probe plan/version. A title alone is not an identity.
- Subtitle correction: selected release identity, embedded reference fingerprint, external subtitle content hash, language, and alignment algorithm version. URL refresh alone must not create a new result; a different file, subtitle, reference, or algorithm must.

The result cache and queue must not store addon tokens, passwords, full install blobs, or signed media URLs. Keep raw diagnostic data redacted and bounded. Apply file-count/size limits and cleanup for subtitle files. A lease and unique `work_key` provide crash recovery and single-flight behavior across duplicate events; expired leases return work to the queue.

## Preparation pipeline

### 1. Candidate discovery and redirect preparation

Fetch candidate listings through each addon with the existing request timeout and normal authentication. Store candidate metadata and provenance, not a promise of playability. Resolve cheap HTTP redirects/HEAD or small playlist probes under strict timeout and public-network protections. Cache positive reachability briefly; cache definitive 404/410 negatives briefly enough to retry after provider repairs. Do not mark a source dead for timeouts, rate limits, DNS hiccups, or 5xx responses. Redirect chains must reject private/local targets and excessive hops. Keep stream segments direct to the origin.

For URLs requiring short-lived signatures, cache resolution as an observation rather than a durable playable URL. If a player receives an expired target, Remux must resolve again on demand. Never let a five-minute cache freshness rule override the provider's shorter signature lifetime.

### 2. Usenet screening

Reuse supported UsenetStreamer/NzbDAV checks before inventing another NNTP client. Existing health-check-auto-advance checks a bounded candidate set and samples article availability; it does not prove every article in every file can be downloaded. NzbDAV's ensure-article-existence applies to newly queued jobs and does not retroactively establish readiness for old completed jobs.

For each NZB file, expose readiness as a confidence/coverage record rather than one boolean: `unknown`, `sampled`, `verified`, or `blocked`, with number/fraction of article IDs checked and timestamp. Use two modes:

- **Playback-time/on-demand:** keep the current distributed 30% sample across the whole file rather than checking only its first portion. Check the beginning, middle and tail, plus evenly spread points. This bounds startup latency while making middle/tail holes more likely to be found.
- **Background:** for warm Usenet candidates, progressively check 100% of the NZB article IDs with the supported article-existence/STAT mechanism. Persist a resumable cursor or checked ranges so job interruption does not restart from zero. Batch where the backend supports batching, share duplicate work, and spread requests over the 15-minute freshness window and provider-wide rate budget. If 100% cannot finish within that budget, retain the exact coverage and continue later; do not label a partial scan complete.

Full existence coverage means every listed article ID was queried; it does **not** mean downloading all article bodies or proving that every body can be fetched and decoded. Successful STAT is only positive evidence according to the provider's known reliability; a confirmed absent required article is stronger negative evidence. Do not download article bodies merely to warm the cache. Foreground requests may use the latest completed 30% or 100% result; an in-progress full scan does not block playback. A confirmed missing article from either mode can mark the source blocked, while timeouts remain `unknown`/partial rather than falsely declaring it broken.

Only hide/skip a source when the selected checker gives sufficiently strong negative evidence (for example, confirmed missing required articles); timeouts and partial samples remain `unknown`/`sampled`. Keep the existing bounded candidate count and global NNTP caps. Record check mode, exact coverage and timestamp so a cached 30% sample is never misrepresented as a full existence test.

### 3. Subtitle preparation

Run only for sources and languages already eligible under `tools/subtitle-alignment/README.md`: an external English/Vietnamese subtitle when a suitable full embedded text track of that language is absent. Share one alignment job among concurrent callers. Extract an embedded text reference once per stable source, run ALASS-only, preserve subtitle text, and validate structure before publishing.

Publish a correction only when non-empty and all validation checks pass. Store accepted corrected output for seven days, keyed by content/source/algorithm identity. Empty, malformed, rejected, timed-out or failed results are never successful cache entries; a short negative-cache window (for example five minutes) can prevent repeated expensive failures, but must preserve the original subtitle and retry after expiry. Never replace the original. Existing `[Auto-synced]` naming should remain attached only to corrections actually served.

The current first request may wait up to the configured synchronous subtitle budget (10 seconds by default) while alignment runs; concurrent requests join the same job. That preserves the chance to return corrected bytes to clients that do not re-request tracks. Background preparation improves warm playback but cannot guarantee a correction will be ready before a first cold play. Keep waiting bounded; if it expires, serve the original with the current diagnostic marker and finish work for a later request.

## Does subtitle matching need a separate container?

The *feature and policy* belong in Remux: it knows the selected media source, embedded tracks, external subtitle request, Jellyfin user/client routes, and cache key. The ALASS computation does not need its own public container as a product feature.

The current separate worker exists to isolate ALASS/native/Python dependencies, CPU and memory limits, crashes, rollout and rollback from Remux's Rust HTTP/API server. Baking the algorithm into the Remux process would couple those failure modes and could let a cold multi-minute reference extraction consume API resources. The practical compromise is to make alignment a first-party Remux subsystem with a durable job queue and cache, while keeping a private, resource-limited worker process at first. Remux owns scheduling and results; the worker is an implementation detail. Later, the worker can be managed as a child process or replaced by a maintained Rust/native library if packaging and resource isolation remain sound. “In Remux” should mean one supported configuration, lifecycle and code path, not necessarily one OS process or container.

Do not make a synchronous HTTP callback the only source of truth. Remux should persist the job and result state; the worker should be restartable and idempotent. If the worker is down, playback continues with original subtitles and a later retry.

## Playback response behavior

- Fresh (<=15 minutes): respond from cache without calling addons or NNTP. A cached aligned subtitle is served immediately for its seven-day lifetime.
- Approaching expiry: the activity scheduler refreshes warm listing/readiness entries around minutes 12–14, with jitter; concurrent requests join the same work.
- Older than 15 minutes: enqueue/coalesce high-priority work when playback requests it; return safe last-known-good data if it is still usable. For a source known expired/dead, resolve/check it on demand and fall through to a healthy alternative rather than blocking on every candidate.
- New correction not ready: use the existing bounded first-request wait; never expose an empty corrected subtitle.
- Refresh failure: preserve last-known-good result, update failure telemetry and use backoff with jitter. A confirmed provider removal or source expiry can invalidate only the affected entry.

Jellyfin responses must remain contract-compatible. Expose debug/freshness details only in Remux-owned metadata/diagnostics, not arbitrary fields in standard Jellyfin objects. Ensure intermediary HTTP caches do not retain a pre-alignment fallback in place of a later correction.

## Limits, observability and safety

Start with conservative per-host limits: bounded queue, few parallel addon requests, existing 2-connection NNTP screening and existing ALASS CPU/memory/time limits. Separate the API executor from background work. Add cancellation when an item is deleted or inputs change, but do not cancel a shared job while another caller needs it. Use exponential backoff with jitter for transient errors and circuit-break failing providers briefly.

Measure cache hit rates and age by data kind; queue depth and oldest job; time from playback start to next-episode readiness; addon/redirect/Usenet/subtitle latency; missing-article sample coverage; subtitle accepted/empty/rejected counts; CPU, reference bytes read and job duration; stale fallbacks and provider failure rates. Never log credentials, full signed URLs, subtitle dialogue, or install blobs.

## Rollout and acceptance tests

1. Add schema, queue and metrics behind a disabled-by-default config flag; verify restart recovery, deduplication, lease expiry, bounded queue, and that the playback API never waits on background-only jobs.
2. Shadow mode: enqueue and measure work but do not change results. Compare cached stream candidates, redirects and Usenet sample decisions to current on-demand behavior.
3. Enable stream-list/redirect warmup first; confirm fresh requests make no redundant addon calls, stale refreshes do not delay playback, expired signed links are renewed, and confirmed dead entries still fall through.
4. Enable Usenet checks next; verify the playback-time 30%-distributed probe across known complete and incomplete releases, then verify the background scan reaches 100%, resumes after interruption, and respects provider budgets. Confirm a missing middle/tail article is found, while a timeout is not mislabeled as missing and partial coverage is never marked complete.
5. Enable subtitle warmup last. Test cold and warm same-source alignment, concurrent requests, empty/malformed output, worker restart, algorithm-version invalidation, external subtitle replacement, and original fallback. Verify non-empty accepted output persists seven days and empty output does not.
6. Exercise next-episode and seven-day recently played scopes, multi-device duplicate playback, queue saturation, provider outage, process restart and rollback. On real players, test source selection, first byte, seek and subtitle picker.

Success means warm playback responses are effectively cache reads; no background work changes or delays active media bytes; first-play cold subtitle behavior remains bounded and correct; incomplete-source screening reduces known failures without hiding transient outages; and queue/runtime resource consumption stays within an explicit budget.

## Upstream retirement map

| Capability | Likely home | What could retire the custom fork code |
| --- | --- | --- |
| Durable episode-aware background queue and warmup policy | Remux core | Upstream Remux adds equivalent supported lifecycle hooks, queue, and bounded scheduler |
| Candidate/redirect cache policy and stale-while-refresh | Remux core + provider addon | Upstream cache/preflight contract plus VNPHIM/provider expiry semantics; prove signed URLs are renewed |
| Usenet article screening and sampled coverage | UsenetStreamer/NzbDAV integration, orchestrated by Remux | Supported checker API gives per-file coverage and stable identity; no remux-specific protocol fork needed |
| ALASS eligibility, result validation, cache keys and Jellyfin subtitle routing | Remux core | Upstream has equivalent source-specific alignment, persistence, non-empty validation, and client delivery |
| ALASS binary/runtime | Private worker implementation initially | Replace only after equivalent dependency/resource isolation is tested; separate process may remain even when code is first-party |
| Remote-HLS item/PlaybackInfo compatibility and Fladder version aliases | Remux API compatibility | Stock upstream passes the documented retirement acceptance matrix with identical VNPHIM and real clients |
| Hotphim domains, redirects and stream construction | VNPHIM (user-owned) | Not a Remux fork patch; maintain and test in VNPHIM |

Track code changes and deployed acceptance evidence in `FORK-RETIREMENT.md`. A clean upstream rebase alone is not retirement evidence.

## Current implementation status (2026-09-23)

The stream-list prefill queue and next-episode priority policy are deployed on `remote-hls-sources` (commits `2d2177ce` and `bac64848`). The live No Pain No Gain S1E18 session confirmed that stream-list refresh jobs are created for the current episode, the next three available episodes, and up to 100 episodes played by the same user in the series during the prior week. The worker completed initial passes and kept the episode rows queued for their 12–14 minute refresh window. E19 was promoted to priority 200 and scheduled ahead of E20–E22 during the live run. Each refresh updates the library cache; it does not download media bytes.

Queue priority policy: the next episode is priority 200, current episode 100, the following two upcoming episodes 80, and other recent episodes 40. Duplicate targets within one playback event keep the highest applicable priority. A later playback event replaces stale queue priority values, so a former “next episode” does not keep its boost indefinitely. The worker skips the usual 0–120 second refresh jitter for priority-200 jobs. Playback-start enqueueing does not block playback; queue work is spaced by two seconds, and active-series refreshes stop after 24 hours without activity. Focused tests passed (2/2), and deployed binary SHA256 is `e9e2e49ecc71a31ce13ff75015f05e8c04fe7521004b87b4b39667896799226d`.

Stream-list freshness defaults to 15 minutes. Empty addon responses retain last-known-good stream rows; transient errors back off. Successful non-empty aligned subtitle results are stored atomically under the Remux data directory for seven days, capped at 128 result files; empty or rejected results are not persisted.

Still not implemented: background refresh of resolved redirect targets, automatic pre-alignment of next-episode subtitles, persistent resumable Usenet article coverage, and background 100% Usenet existence scans. The current 30% playback triage belongs to UsenetStreamer; a complete background scan needs a supported per-file progress API. Continue observing the E19 refresh timing and cache freshness during real playback.
