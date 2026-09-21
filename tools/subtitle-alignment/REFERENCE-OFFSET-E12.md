# E12 embedded reference is late relative to audio — 2026-09-21

## Root cause and correction to prior conclusions

The HHWEB Usenet No Pain No Gain S1E12 embedded Chinese track is approximately
15 seconds later than the actual audio. Its format/video/audio start_time values
are zero. Fresh extraction reproduces the saved reference timestamps, ruling out
a stale extracted reference as the cause. Previous checks matching external cues
to that reference (including 8ms/1ms reports) proved delivery, NOT audio sync.
Those earlier claims should not be interpreted as playback accuracy.

A one-off LOCAL speech transcription of three short audio windows independently
identified dialogue near 07:18, 08:40 and 33:49. It ran in a separate diagnostic
venv on nimo; no audio was sent to cloud inference, no speech model was installed
in the production worker, and there is no model-based production matching.
Diagnostic tool: https://github.com/SYSTRAN/faster-whisper (base, CPU/int8).
Recognition has small transcription/timestamp errors; these are approximate
speech anchors, not millisecond ground truth. Private audio/transcripts remain
outside Git. No full-episode audio verification was performed.

| Dialogue | Audio diagnostic | Old embedded reference | New Vietnamese |
| --- | --- | --- | --- |
| Decides to try harder | 436.88–438.76s | About 15s later | 437.109–439.189s |
| Earned some money | Around 520–521s (clip begins mid-utterance) | 535.541–536.541s | 520.549–521.549s |
| Only rent, not sell | 2028.76–2029.88s | 2043.779–2045.059s | 2028.613–2030.060s |

Reverting to original Vietnamese alone is insufficient: it fits the early section
but is about 18s early later. ALASS's old shifts were +15.079s then +33s. Correcting
the embedded reference by -15s before matching yields +0.079s then +18s, retaining
the real release-cut difference without inheriting the bad reference delay.

## Narrow, reviewed override

Worker v5 `embedded-text-v5-reference-offset` reads
`reference-timing-overrides.json` from ALIGN_RUNTIME. Keys are SHA256 of compact
UTF-8 JSON `[start_ms,end_ms,text]` cue arrays parsed with pysubs2; values are
reviewed integer millisecond shifts, bounded to +/-120000ms. No filename/title
heuristic is used. A changed track, timing or dialogue yields a different key.
Invalid overrides/timings fail rather than applying a guessed correction.
The shift participates in the worker result key. Restart Remux after changing
worker/override data to clear its in-memory results.

The one shipped entry is -15000ms for exact reference fingerprint
`6ca618c127606651a4f0898e5d12aac5e4d2884b92e3eb28b2d52d8c85c6dae9`.
Install the JSON into `/var/lib/remux-subtitle-alignment/` alongside the existing
runtime data (readable by service user). No other reference gets an offset.
This is a proven-release exception, NOT automatic detection of bad embedded tracks.
ALASS remains best effort elsewhere. If an upstream/provider repair changes this
reference, its hash no longer matches. Remove this entry when it is no longer
needed; preserve the evidence when testing retirement.

## Verification and deployment

17 worker unit tests pass, including offset-before-alignment, cache invalidation
when offset changes, and no effect on another track. Local E12 matching 1.256s,
1,070 cues preserved. Live WebVTT SHA256
`2d53115160f33fc88c0977c36cdb8d3251904a0eeb9bc74e9114a726052606b8`;
original bypass still works. Worker SHA256
`f6d6f01e759566fd5281af682c5e09b42b5dd4a2f12271adb76aa7c432ba321f`.
Remux binary/synchronous delivery unchanged. Worker and Remux restarted, healthy.

Vidhub was closed and reopened. At paused position 495.4881929s its visible cue
matches the new 494.092–496.259s cue; the previous output placed it at
509.092–511.259s. This proves the new file reached the iPad. Do not extrapolate
this delivery observation into full-episode perceptual/audio verification.

Backup worker: `/root/remux-patch/worker-pre-reference-offset.py` on nimo.
Restoring old worker and restarting Remux would restore the known bad E12 timing;
prefer reviewing/removing a specific override with fresh audio evidence.
