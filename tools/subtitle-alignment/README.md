# Embedded-text subtitle alignment (optional fork feature)

## Responsibility and retirement

Remux knows the **selected media source** and serves the external subtitle through
its existing authenticated Jellyfin subtitle routes. A separate private Python
worker aligns text and returns a validated SRT. vnphim still discovers subtitles;
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
2. Prefer an embedded full text track in the external subtitle's language;
   otherwise try other embedded text tracks (at most two). Skip forced-only and
   bitmap tracks. Absence of a usable reference preserves the original.
3. Remux extracts the reference with original container timestamps. Persistent
   reference cache: guarded provider release + track, seven days, at most
   32 files of 2 MB. Temporary files are removed on completion/cancellation.
4. ALASS 2.0.0 proposes piecewise shifts, accommodating different release cuts.
5. A pinned local multilingual MiniLM ONNX model independently finds distinctive
   dialogue anchors. At least 25 anchors/8% of cues, coverage in eight of ten time
   bins, 85% within 2.5 seconds, and at least 60% in each represented bin are
   required. Changed words, invalid times, and weak matches are rejected.
6. Remux serves accepted text as SRT, VTT, or Jellyfin JSON using the usual route.
   Other requested output formats remain original. Original subtitles are always
   retrievable by appending `&remux_original=true` (or `?` without an existing query).

This is **confidence-gated best effort**, not a promise of perfect timing or every
language. Different translations, split/merged dialogue, credits, unsupported
languages, and missing scenes can defeat matching. Semantic residuals are a
validation signal, not a measurement of every spoken word's timing. No reference
means no correction; a broken video cannot be repaired by subtitle alignment.

## Latency, bandwidth, and bounds

One job at a time per remux process. Requests never wait for cold extraction or
inference: they get the original while the background job runs. **Re-select the
subtitle or reopen playback after processing completes.** A client holding an
already loaded subtitle cannot be updated by the server. No background scan of
every library item/version is started. Cold jobs can take several minutes.

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

Response header `X-Remux-Subtitle-Alignment` reports `aligned`, `pending`, `busy`,
`rejected`, `unavailable`, `no-reference`, `unsupported`, `disabled`, or `original`.
Alignment responses use `private, no-store` so an intermediary cannot retain a
cold fallback after a correction becomes ready. The worker requires a bearer
token, binds privately, and has no public reverse-proxy route.

## Deployment

- Code: `worker.py`; Python dependencies: `requirements.txt` (exact deployed
  environment in `requirements-lock.txt`). The included systemd service is the
  nimo deployment template; adjust its addresses/paths for another host.
- ALASS: <https://github.com/kaegi/alass/releases/tag/v2.0.0>.
- Model: <https://huggingface.co/sentence-transformers/paraphrase-multilingual-MiniLM-L12-v2>.
- Model revision: `e8f8c211226b894fcb81acc59f3b34ba3efd5f42`.
- Fetch `onnx/model_quint8_avx2.onnx` as `model/model.onnx` and `tokenizer.json`
  as `model/tokenizer.json`. Fetch the official `alass-linux64` binary as `alass`.
- Verify the hashes in `artifacts.sha256`; model/executable are not committed.
- Create a random token file in the worker state directory, readable only by the
  service account. Supply the same token to remux in a read-only secret file.
- Run with `ALIGN_RUNTIME` pointing at that directory and `ALIGN_BIND` at a
  private interface; port 8791. `model/`, `alass`, `token`, and writable `cache/`
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
