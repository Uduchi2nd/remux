# Embedded-text subtitle alignment (optional fork feature)

## Current mode: ALASS-only (2026-09-21)

Production worker `embedded-text-v4-alass-only` does not load or run the multilingual model. Only external English/Vietnamese subtitles missing as a full embedded language are eligible. See [ALASS-ONLY.md](ALASS-ONLY.md) for timing measurements and the deliberate removal of semantic validation. Older quality reports below describe historical model-based experiments.

## Responsibility and retirement

Remux knows the **selected media source** and serves the external subtitle through
its existing authenticated Jellyfin subtitle routes. A separate private Python
worker adjusts timestamps and returns a structurally checked SRT. vnphim still discovers subtitles;
it does not know which debrid/Usenet release the player ultimately selects.
No player changes, audio transcription, translation API, or video transcoding are
required. Subtitle words are not translated or rewritten.

Keep this feature separate from `remote-hls-sources` playback fixes. Disable it by
removing `SUBTITLE_ALIGNMENT_URL` and `SUBTITLE_ALIGNMENT_TOKEN_FILE`, then
recreating remux. Stop the worker if unused. Upstream retirement requires testing
the same selected-release timing, sidecar/addon routing, VTT/SRT/JSON delivery,
source-specific cache isolation, and failure fallback. Do not retire it merely
because upstream discovers external subtitles correctly.

## Request flow

1. An addon or sidecar subtitle request identifies the selected source.
2. Only external English or Vietnamese is eligible. If a non-forced embedded
   subtitle already has that language (including bitmap), leave the external
   subtitle unchanged and skip extraction/alignment. Otherwise try full embedded
   text tracks (at most two); exclude forced/signs-only and bitmap references.
   Track selection remains a player choice; this does not hide external tracks.
3. Remux extracts the reference with original container timestamps. Persistent
   reference cache: guarded provider release + track, seven days, at most
   32 files of 2 MB. Temporary files are removed on completion/cancellation.
4. ALASS 2.0.0 proposes piecewise shifts, accommodating different release cuts.
5. Validate cue count, unchanged text, nonnegative/positive-duration timestamps,
   and chronological cue starts. There is **no language-model inference or
   semantic-confidence gate**. ALASS subprocess budget is 20 seconds.
6. Remux serves accepted text as SRT, VTT, or Jellyfin JSON using the usual route.
   Other requested output formats remain original. Original subtitles are always
   retrievable by appending `&remux_original=true` (or `?` without an existing query).

This is **ALASS-only best effort**, not a promise of perfect timing. Structural
checks cannot detect wrong-title references or every bad match. Different cuts,
translations, and missing scenes can defeat timing-pattern matching. Original
subtitles remain available through the bypass. The prior semantic thresholds
are retained only in offline research/testing functions, never in Engine.align.

## Latency, bandwidth, and bounds

One job at a time per remux process. Subtitle requests now wait for the corrected
result, including requests that encounter an existing pending job. The shared
request budget is `subtitle_alignment_wait_seconds` (environment variable
`SUBTITLE_ALIGNMENT_WAIT_SECONDS`), default 10 seconds, capped at 120 seconds;
zero opts out of waiting. Queueing, reference extraction and worker time all use
this one budget. Cached corrections return immediately. A completed rejection or
failure returns the original; an expired request budget returns the original with
`wait-timeout`, while the bounded background job can finish for later requests.
This avoids forcing a reload for normal fast alignment, but a genuinely cold
remote extraction exceeding the budget still needs a new subtitle request after
completion. A loaded player track cannot be replaced by HTTP headers alone.
No library-wide scan or language model is added. ALASS itself measured about
0.3–3.4 seconds on the fixtures; remote extraction can take much longer.

Reading an embedded track from a remote MKV can require reading much or all of
the video once. No video file is saved and playback remains direct, but this
extraction traffic is real. The text cache avoids repeating it for other external
subtitles on that release. For HTTP sources with a filename and positive file size,
the key uses remux's stable provider-release ID plus filename, size, probed duration,
and available torrent/NZB identity. Renewed signed URLs therefore retain the
correction. Unknown sources retain the full descriptor and may conservatively
miss the cache on URL renewal. A provider silently replacing a file with identical
identity, size, and duration can defeat these metadata guards; the cache is not a
full-video content hash. Extraction timeout is 120 seconds per reference, worker request
timeout 300 seconds, total job limit 720 seconds. A timeout retains the original.

Worker cache: 200 results, 14 days, content/algorithm-version keys. Remux result
cache: weighted by text size, 24 hours; temporary failures retry after ten minutes.
Restart remux after changing the worker algorithm to invalidate its in-memory
results; the persistent embedded-reference cache survives. Worker v2 normalizes
HTTP CRLF/LF line endings and a leading UTF-8 BOM before parsing and hashing.
Both input tracks are limited to 2 MB and 30–5,000 cues. Cached references occupy
at most about 64 MB; worker data lives on nimo, outside the small remux LXC disk.
No external media URL, addon token, or API key is sent to the worker.

Response header `X-Remux-Subtitle-Alignment` reports `aligned`, `wait-timeout`,
`rejected`, `unavailable`, `no-reference`, `unsupported`, `disabled`, `original`,
`language-skipped`, or `embedded-language`.
Alignment responses use `private, no-store` so an intermediary cannot retain a
cold fallback after a correction becomes ready. The worker requires a bearer
token, binds privately, and has no public reverse-proxy route.

## Deployment

- Code: `worker.py`; Production dependencies: `requirements-runtime.txt`; `requirements.txt` and
  `requirements-lock.txt` additionally support historical offline model tests. The included systemd service is the
  nimo deployment template; adjust its addresses/paths for another host.
- ALASS: <https://github.com/kaegi/alass/releases/tag/v2.0.0>.
- Fetch the official `alass-linux64` binary as `alass`; verify its entry in
  `artifacts.sha256`. No model/tokenizer download is required for production.
  Existing model artifacts may remain on disk for offline comparisons.
- Create a random token file in the worker state directory, readable only by the
  service account. Supply the same token to remux in a read-only secret file.
- Run with `ALIGN_RUNTIME` pointing at that directory and `ALIGN_BIND` at a
  private interface; port 8791. `alass`, `token`, and writable `cache/`
  live there. Run as a dedicated unprivileged user with resource/network limits.
- Remux configuration: `SUBTITLE_ALIGNMENT_URL=http://PRIVATE_HOST:8791/align`
  and `SUBTITLE_ALIGNMENT_TOKEN_FILE=/run/secrets/subtitle-alignment-token`.
  Both default to absent, leaving stock behavior.

Installed on nimo: service `remux-subtitle-alignment`, code/venv under
`/opt/remux-subtitle-alignment`, private state under
`/var/lib/remux-subtitle-alignment`; private bind `10.10.10.1:8791`, allowed source
`10.10.10.13` (LXC 111). Memory cap 2 GB, CPU cap 1.5 cores. Production compose and
binary are backed up before deployment; no secrets belong in this repository.

## Verification, 2026-09-20

Real private fixture: *No Pain No Gain* E12, external Vietnamese vs HHWEB Usenet
embedded Chinese. All 1,070 external cues retain their text.

| External original | Corrected | Independently checked embedded reference |
| --- | --- | --- |
| 10:59.190 | 11:14.269 | 11:14.261 |
| 33:37.710 | 34:10.710 | 34:10.700 |
| 33:43.950 | 34:16.950 | 34:16.940 |

504 distinctive multilingual anchors: median absolute residual 0.010 s, 85.5%
within 2.5 s; 90th percentile 3.717 s. The uncorrected file has median residual
32.990 s and zero anchors within 2.5 s. A scrambled reference is rejected.
Same-text validation accepts 1,033 anchors at zero residual. Running ALASS on
an already synchronized identical subtitle shifts at most 1 ms.

`python -m unittest -v test_worker` covers changing offsets, wrong/ambiguous
anchors, an incorrect tail despite good global score, text changes, and invalid
input. `verify_fixture.py` is an operator-only regression check using private
fixtures outside the repository; never commit subtitle contents or signed URLs.
Rust tests cover source/content isolation, failure byte preservation, bitmap
exclusion, and the existing Jellyfin subtitle route/sidecar behavior.

The first live HTTP trial was conservatively rejected because CRLF line endings
left carriage returns in parsed dialogue while file-based tests normalized them.
No incorrect correction was served. Worker v2 fixes that representation mismatch;
an eighth unit test verifies CRLF/LF/BOM equivalents preserve identical dialogue.

A second live trial exposed provider URL renewal while matching was running.
The matcher accepted the correction, but exact-URL cache keys made it hard to
retrieve consistently and repeated reference scans. The guarded release key fixes
that; dedicated tests cover URL renewal, filename/size/duration changes, unknown
URL identities, and header-map ordering.

For the v1-to-v2 release-key upgrade, `migrate_reference_cache.py --data-dir
/opt/remux/data` can reuse a reference only when its old key matches the exact
currently stored descriptor. It atomically renames the cache file, preserves its
age, and makes no media request. Run with alignment disabled during migration.

Final live server verification: all 1,070 cues preserve dialogue; SRT, VTT, and
Jellyfin JSON return identical corrected timestamps. The original bypass returns
the unmodified timeline. Final server binary SHA256: `ad335bb887a29154cf016cd7a0a13d4853443098191aa1696644e8a2700e636f`. Twelve targeted Rust
tests and eight worker tests pass. Production service and the live subtitle route
were checked after deployment.

Independent TorBox/embedded-English control: all 1,070 cues retain their text;
median and maximum absolute start-time shift are both 1 ms. Its three dialogue
anchors stay on their original timeline, distinct from the Usenet correction.
A fresh physical iPad Infuse session played the explicitly selected Usenet release
and rendered Vietnamese subtitles. Precise audio/lip-sync was not measured from
the remote screen; quantitative timing verification is against the embedded text
reference, including points near minutes 11 and 34.

## Broader quality and latency follow-up

See [QUALITY.md](QUALITY.md) for the four-title, full-timeline recovery matrix, false negatives, exact-dialogue fast path, formatting fix, and remaining cold-start costs. Worker v3 preserves the existing E12 correction; it does not promise perfect alignment or instant cross-language matching.
