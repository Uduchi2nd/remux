# Bounded synchronous subtitle delivery

## Problem and fix

Previously every uncached eligible request returned the original immediately, even
when ALASS finished a second later. Players retained that loaded subtitle and did
not automatically request the correction. Remux now waits for a terminal result
on the first request and joins an already pending result for concurrent requests.
Queueing and processing share a 10-second default request budget. A configurable
`subtitle_alignment_wait_seconds` field participates in normal Config/env loading;
the effective value is capped at 120 seconds, and zero disables waiting.

Cached results remain immediate. Failures/rejections retain original bytes. If the
request budget expires, `X-Remux-Subtitle-Alignment: wait-timeout` explicitly marks
original fallback; the bounded background job continues for a later request.
This does not promise cold remote extraction always finishes before the deadline,
or replace an original already loaded by a player. English/Vietnamese eligibility,
source isolation, no-model ALASS matching and direct video delivery are unchanged.
No worker or VNPHIM modification is needed. Disabling alignment URL/token disables
the entire feature independently of remote-HLS/client compatibility patches.

## Verification

Regression checks cover pending-to-ready first response, bounded pending timeout,
and failure preserving original text, alongside existing source/language tests.
Live deployment results and binary checksum will be appended after verification.

## Deployed verification

Code commit `2bbee957`; 16 targeted Rust subtitle tests passed and release build
completed successfully. Deployed binary SHA256
`ea23d9eaa50c198d6c4fd0089f087de114b5cc95072c6e651f9abc02a1819f3e`.
Worker unchanged, ALASS-only/no model. Health check passed.

Live E12 test: restarted Remux to empty its result cache, temporarily set aside all
seven worker results whose 1,070 texts matched E12, and kept the embedded reference
cache warm. Three simultaneous requests returned `aligned` in 2.045–2.050 seconds,
with identical corrected WebVTT hashes. One new worker result was created. Next
request returned the same correction in 75ms. This demonstrates first-response
waiting through real computation and concurrent job sharing, not just a warmed
Remux cache. It does not measure remote reference extraction from a cold video.
Sanitized evidence: synchronous-delivery-live.json. Cache backups are private on
nimo; unaffected results were restored. Original bypass and same-language skip
remain separately verified.

Rollback: previous binary is `/root/remux-patch/remux-pre-sync-subtitles` on nimo,
SHA256 `db358ae4227324ad688dd09eedb7b2e02e220f1a759b419d9ecb6eff15c89fd0`.
Stop container, restore that binary into LXC111 `/opt/remux/remux-server-patched`,
chmod 755, and recreate the existing compose service. Worker/config need no change.
This restores immediate-original asynchronous delivery, while retaining ALASS-only
matching. Setting the wait setting to zero also opts out of request waiting.
