# Subtitle alignment quality and latency — 2026-09-21

## Scope and method

Real embedded subtitle pairs were extracted from No Pain No Gain S01E11 (Vietnamese/Chinese, 939 external cues), Chad Powers S01E04 (Spanish/English, 350), Supergirl (Spanish/English, 985), and Die My Love (Spanish/English, 807). Two movie collections exceeded a 180-second home-side budget; successful seedbox extraction took 36 and 62 seconds. Only subtitle text was retained. Extraction is a separate cost from matching and may scan the whole video.

For each pair, test the original timing, an added 17-second delay, a mid-title change from +15 to +33 seconds, and a 4% timing stretch. Compare every corrected cue against that external track's pre-perturbation timestamps. This measures recovery of known timing, not audio/lip-sync or a proof that every original translation is correct. Text and cue count must remain unchanged. Three cross-title reference controls must reject. Actual vnphim E11 and the original E12 case are tested separately.

## Results

| Title | Cross-language fixed-delay/cut recovery, worst cue | 4% stretch recovery, worst cue | Semantic acceptance |
|---|---:|---:|---|
| No Pain No Gain E11 | 0 ms | 280 ms | all four accepted |
| Chad Powers E4 | 82 ms | 640 ms | all four rejected |
| Supergirl | 222 ms | 1,366 ms | all four rejected |
| Die My Love | 1 ms | 1,199 ms | all four accepted |

The rejected controls had reasonable timing proposals but the semantic model confused some translated dialogue matches. These are false negatives on the controlled pairs, not successful corrections. Thresholds were not relaxed. All three cross-title controls rejected. Actual vnphim E11 also rejected against both Chinese and Vietnamese embedded references; original subtitles remain the fallback. This is a remaining coverage limitation, not a promise of universal alignment.

Twelve same-dialogue controls (each title unchanged, +17 seconds, and +15/+33 seconds) accepted through the new exact path. All 3,081 cues across the four titles retained their text; maximum recovered timing error was 1 ms. Uncached matcher times were approximately 0.6–4.8 seconds. These controls use the same dialogue edition with synthetic timing changes, not independently obtained external translations.

Original E12 regression: all 1,070 cues retained; the three previously checked anchors remain within 8/10/10 ms of the embedded reference. It still uses semantic validation. Its new run took 79 seconds with other quality tests running, so this update does **not** make every difficult case fast. No new physical-player/audio-sync verification was performed for the four added titles.

## Changes found by testing

- Preserve SSA-style positioning tags during SRT serialization. Previously `{\an8}` was stripped before ALASS and valid subtitles were falsely rejected as changed dialogue. Validate ALASS's serialized cue mapping, then copy only proposed timestamps onto the original cues. Keep tags in the returned SRT. Words are not regenerated.
- Conservative exact-dialogue fast path: at least 40 unique matching cues and 60% of all cues, 9/10 timeline bins with at least three anchors, at least 99% globally and 95% in each represented bin within 250 ms (start) / 500 ms (end). Repeated lines and uncertain cases continue to semantic validation.
- Batch semantic contexts by token length to reduce padding computation; cache at most six embedding arrays in memory. Timings are excluded from the embedding key because contexts contain dialogue only. The cache is per process/model and capped at approximately 46 MB for maximum-size inputs.
- Optimized encoding on the TV samples measured 42 versus 60 seconds and 23 versus 44 seconds. These are host-load-dependent measurements, not an SLA. Quantized batching can slightly change similarity scores; acceptance and the original E12 case were rechecked.
- Worker version is `embedded-text-v3`; restart the worker and remux when deploying to clear old result caches. The remux binary/configuration does not change.

## Reproduce

`quality_matrix.py MANIFEST OUTPUT` uses the installed runtime selected by `ALIGN_RUNTIME`. Manifest is a JSON array with `title`, `external`, `reference`, and optionally `additional_external`; paths resolve relative to the manifest. Supply known synchronized subtitle pairs. The script never fetches media, prints subtitle dialogue, or requires media URLs. `test_worker.py` covers 12 small regression cases. The detailed sanitized measurements are in `quality-results-2026-09-21.json`.

Private real fixtures and operator execution scripts remain under `/root/remux-quality` on nimo. Do not commit subtitle contents, provider URLs, tokens, or credentials. Worker rollback is restoring its pre-v3 backup and restarting both services. Keep the existing remux fork retirement notes: this optional subtitle worker can be disabled independently of remote-HLS and Vidhub fixes.

## Deployment

Deployed worker v3 on nimo; service active and remux healthy after clearing old result caches. An authenticated uncached live same-dialogue request completed in 0.33 seconds. Worker SHA256: `07b31a373f9f483cf3168427ae6b2f3faa8d444b92d58142595f95151765c5fc`. Backup: `/opt/remux-subtitle-alignment/worker.py.pre-v3-20260921`. Production remux binary remains `ad335bb887a29154cf016cd7a0a13d4853443098191aa1696644e8a2700e636f`.

Post-deployment E12 live SRT/VTT/Jellyfin JSON verification passed: all 1,070 cues preserved, identical corrected timestamps, original bypass unchanged, three anchors 8/10/10 ms. Production cold semantic work was roughly 90 seconds under the service CPU quota; the exact fast path does not eliminate this remaining latency.
