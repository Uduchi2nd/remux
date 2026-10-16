"""Virtual-file ranges must preserve complete video and audio packets."""
import asyncio
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import AsyncMock, patch

from starlette.requests import Request

import server


def request(method="GET", **headers):
    req = Request({"type": "http", "method": method, "path": "/file.ts",
                    "query_string": b"", "headers": [(k.encode(), v.encode()) for k, v in headers.items()]})
    req.is_disconnected = AsyncMock(return_value=False)
    return req


async def body(response):
    return b"".join([part async for part in response.body_iterator])


class SeekableFileTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        for name in ("match", "hls", "audio"):
            (self.root / name).mkdir()
        for attr, value in {"MATCH": self.root / "match", "HLS": self.root / "hls",
                            "AUDIO": self.root / "audio", "_jobs": {}}.items():
            p = patch.object(server, attr, value)
            p.start()
            self.addCleanup(p.stop)
        for owner, name in [(server.dubmux, "resolve_url"), (server.MEDIA_READS, "acquire")]:
            p = patch.object(owner, name, side_effect=AssertionError("repair attempted a provider read"))
            p.start()
            self.addCleanup(p.stop)
        self.key = "dub__hq"
        self.session = self.root / "hls" / self.key
        self.session.mkdir()
        self.payloads = [b"\x47" + b"a" * 187, (b"\x47" + b"b" * 187) * 4]
        for i, data in enumerate(self.payloads):
            (self.session / f"seg{i:05d}.ts").write_bytes(data)
        (self.session / "index.m3u8").write_text(
            "#EXTM3U\n#EXTINF:6,\nseg00000.ts\n#EXTINF:6,\nseg00001.ts\n#EXT-X-ENDLIST\n")
        (self.session / ".done").write_text("complete")
        self.layout = self.root / "match" / ("file-" + self.key + ".json")
        self.layout.write_text(json.dumps({"slots": [376, 376], "from": "raw"}))

    def serve(self, req, slots, etag=None):
        offs, n = [], 0
        for slot in slots:
            offs.append(n)
            n += slot
        return server._serve_slots(req, offs, slots, n,
            lambda i: self.session / f"seg{i:05d}.ts", "fixture", etag)

    def test_completed_layout_is_repaired_without_losing_packet_bytes(self):
        offsets, slots, total = server._file_layout(self.key, "dub")
        self.assertEqual(json.loads(self.layout.read_text())["from"], "session-exact")
        response = self.serve(request(), slots)
        data = asyncio.run(body(response))
        self.assertEqual(len(data), total)
        for i, payload in enumerate(self.payloads):
            self.assertEqual(data[offsets[i]:offsets[i] + len(payload)], payload)
        saved = self.layout.read_bytes()
        self.assertEqual(server._file_layout(self.key, "dub"), (offsets, slots, total))
        self.assertEqual(saved, self.layout.read_bytes())

    def test_healthy_existing_layout_and_cache_only_lookups_do_not_write(self):
        before = self.layout.read_bytes()
        self.assertEqual(server._file_layout(self.key, "dub", create=False)[1], [376, 376])
        self.assertEqual(before, self.layout.read_bytes())
        self.layout.write_text(json.dumps({"slots": [376, 940]}))
        before = self.layout.read_bytes()
        self.assertEqual(server._file_layout(self.key, "dub")[1], [376, 940])
        self.assertEqual(before, self.layout.read_bytes())

    def test_range_crossing_slot_padding_preserves_the_exact_bytes(self):
        slots = [376, 940]
        expected = self.payloads[0] + server.TS_NULL + self.payloads[1] + server.TS_NULL
        for a, b in [(37, 800), (188, 375), (377, 1315)]:
            with self.subTest(a=a, b=b):
                response = self.serve(request(range=f"bytes={a}-{b}"), slots)
                self.assertEqual(response.status_code, 206)
                self.assertEqual(response.headers["content-range"], f"bytes {a}-{b}/1316")
                self.assertEqual(asyncio.run(body(response)), expected[a:b+1])

    def test_oversized_first_segment_fails_before_success_headers(self):
        with self.assertRaises(server.HTTPException) as ctx:
            self.serve(request(range="bytes=376-751"), [376, 376])
        self.assertEqual(ctx.exception.status_code, 503)

    def test_later_overflow_aborts_instead_of_delivering_truncated_media(self):
        response = self.serve(request(), [376, 376])
        delivered = []

        async def collect():
            async for part in response.body_iterator:
                delivered.append(part)

        with self.assertRaises(server.HTTPException):
            asyncio.run(collect())
        self.assertEqual(b"".join(delivered), self.payloads[0] + server.TS_NULL)

    def test_changed_layout_etag_prevents_reusing_old_range_offsets(self):
        for supplied, status, length in [('"old-layout"', 200, 1316), ('"current-layout"', 206, 188)]:
            response = self.serve(request(range="bytes=376-563", **{"if-range": supplied}),
                                  [376, 940], '"current-layout"')
            self.assertEqual(response.status_code, status)
            self.assertEqual(len(asyncio.run(body(response))), length)
            self.assertEqual(response.headers["etag"], '"current-layout"')
            self.assertIn("no-cache", response.headers["cache-control"])

    def test_abandoned_range_does_not_read_or_produce_the_following_segments(self):
        req = request()
        req.is_disconnected = AsyncMock(side_effect=[False, False, True])
        produced = []

        def segment(i):
            produced.append(i)
            return self.session / f"seg{i:05d}.ts"

        response = server._serve_slots(req, [0, 376], [376, 940], 1316, segment, "fixture")
        self.assertEqual(asyncio.run(body(response)), self.payloads[0] + server.TS_NULL)
        self.assertEqual(produced, [0])

    def test_finished_in_memory_job_reports_the_repaired_file_size(self):
        (self.root / "match" / (self.key + ".json")).write_text(json.dumps({"verdict": "accept"}))
        server._jobs[self.key] = {"status": "ready", "result": {"verdict": "accept", "file_size": 752}}
        result = server.status("dub", "hq")["result"]
        self.assertEqual(result["file_size"], server._file_layout(self.key, "dub")[2])
        self.assertGreater(result["file_size"], 752)


if __name__ == "__main__":
    unittest.main()
