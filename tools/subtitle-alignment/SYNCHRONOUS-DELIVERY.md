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
