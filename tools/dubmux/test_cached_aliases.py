"""Refreshed provider IDs must not re-download an already completed VN mux."""
import asyncio
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import AsyncMock, patch

from starlette.requests import Request

import server


class CachedAliasTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        for name in ("audio", "match", "hls"):
            (self.root / name).mkdir()
        for name in ("AUDIO", "MATCH", "HLS"):
            p = patch.object(server, name, self.root / name.lower())
            p.start()
            self.addCleanup(p.stop)
        for owner, name in [(server.MEDIA_READS, "check"), (server.MEDIA_READS, "acquire"),
                            (server.dubmux, "resolve_url"), (server, "_start_mux"),
                            (server, "_reject_pair")]:
            p = patch.object(owner, name, side_effect=AssertionError("cached alias attempted paid work"))
            p.start()
            self.addCleanup(p.stop)
        self.old = "old-dub__old-hq"
        self.new = "new-dub__new-hq"
        self.aligned = self.old + ".aligned.m4a"
        (server.MATCH / self.aligned).write_bytes(b"existing aligned audio")
        self.match = {"verdict": "accept", "video_check": "ok", "video_duration": 12.0,
                      "lag": 0.0, "aligned": self.aligned, "sync_check": {"ok": True},
                      "video_url": "https://offline.invalid/must-not-download"}
        self.alias = dict(self.match, aliased_from=self.old, video_check=None)
        self.write_match(self.old, self.match)
        self.write_match(self.new, self.alias)
        self.directory = server.HLS / self.old
        self.directory.mkdir()
        (self.directory / ".done").write_text("complete")
        (self.directory / "index.m3u8").write_text(
            "#EXTM3U\n#EXTINF:6,\nseg00000.ts\n#EXTINF:6,\nseg00001.ts\n#EXT-X-ENDLIST\n")
        self.payloads = [b"\x47" + b"a" * 187, (b"\x47" + b"b" * 187) * 3]
        for i, data in enumerate(self.payloads):
            (self.directory / f"seg{i:05d}.ts").write_bytes(data)
        self.layout = server.MATCH / ("file-" + self.old + ".json")
        self.layout.write_text(json.dumps({"slots": [188, 564], "from": "session-exact"}))
        self.stale_layout = server.MATCH / ("file-" + self.new + ".json")
        self.stale_layout.write_text(json.dumps({"slots": [188, 188], "from": "raw"}))

    def write_match(self, key, match):
        (server.MATCH / (key + ".json")).write_text(json.dumps(match))

    def lookup(self):
        return server.cached_playback("new-dub", "new-hq")

    def snapshot(self):
        return {str(p.relative_to(self.root)): p.read_bytes()
                for p in self.root.rglob("*") if p.is_file()}

    def request(self, method="GET", **headers):
        request = Request({"type": "http", "method": method, "path": "/file.ts",
                           "query_string": b"", "headers": [(k.encode(), v.encode()) for k, v in headers.items()]})
        request.is_disconnected = AsyncMock(return_value=False)
        return request

    def test_cached_alias_uses_repaired_donor_layout_without_writing_or_copying(self):
        before = self.snapshot()
        self.assertEqual(self.lookup(), {"status": "ready", "cached": True,
            "result": {"video_check": "ok", "file_ready": True, "file_size": 752}})
        self.assertEqual(before, self.snapshot())
        self.assertFalse((server.HLS / self.new).exists())

    def test_head_and_seek_read_cached_packets_without_budget_or_resolver(self):
        head = server.seekable_file("new-dub", "new-hq", self.request("HEAD"))
        self.assertEqual(head.status_code, 200)
        self.assertEqual(head.headers["content-length"], "752")
        response = server.seekable_file("new-dub", "new-hq", self.request(range="bytes=188-375"))

        async def collect():
            return b"".join([part async for part in response.body_iterator])

        self.assertEqual(response.status_code, 206)
        self.assertEqual(response.headers["content-range"], "bytes 188-375/752")
        self.assertEqual(asyncio.run(collect()), self.payloads[1][:188])
        self.assertTrue((self.directory / ".touched").is_file())
        self.assertFalse((server.HLS / self.new).exists())

    def test_preparation_reports_current_cached_video_check_and_size(self):
        result = server._playback_result(self.new, self.alias)
        self.assertEqual(result["video_check"], "ok")
        self.assertEqual(result["file_size"], 752)
        self.assertEqual(result["file_ready"], True)

    def test_nested_aliases_follow_the_same_completed_representation(self):
        middle = "middle-dub__middle-hq"
        self.write_match(middle, self.alias)
        self.write_match(self.new, dict(self.alias, aliased_from=middle))
        self.assertEqual(self.lookup()["result"]["file_size"], 752)

    def test_missing_or_failed_completed_media_does_not_become_cached(self):
        (self.directory / ".done").unlink()
        self.assertEqual(self.lookup()["status"], "unknown")
        (self.directory / ".done").write_text("complete")
        (self.directory / ".failed").write_text("bad")
        self.assertEqual(self.lookup()["status"], "unknown")

    def test_incompatible_unchecked_or_rejected_donor_is_not_reused(self):
        (server.MATCH / "different.m4a").write_bytes(b"different alignment")
        for change in [{"video_check": "failed"}, {"verdict": "reject:sync"},
                       {"sync_check": {"ok": False}}, {"video_duration": 13.0},
                       {"aligned": "different.m4a"}, {"aligned": "missing.m4a"}]:
            with self.subTest(change=change):
                self.write_match(self.old, dict(self.match, **change))
                before = self.snapshot()
                self.assertEqual(self.lookup()["status"], "unknown")
                self.assertEqual(before, self.snapshot())
        self.write_match(self.old, self.match)
        self.write_match(self.new, dict(self.alias, sync_check={"ok": False}))
        self.assertEqual(self.lookup()["status"], "unknown")

    def test_cycles_and_parent_path_escape_are_not_followed(self):
        for parent in [self.new, "../escape__hq"]:
            with self.subTest(parent=parent):
                self.write_match(self.new, dict(self.alias, aliased_from=parent))
                self.assertEqual(server._session_dir(self.new), server.HLS / self.new)
        self.write_match(self.old, dict(self.match, aliased_from=self.new))
        (self.directory / ".done").unlink()
        self.write_match(self.new, self.alias)
        self.assertEqual(server._session_dir(self.new), server.HLS / self.new)

    def test_missing_donor_layout_stays_unknown_without_recreating_it(self):
        self.layout.unlink()
        before = self.snapshot()
        self.assertEqual(self.lookup()["status"], "unknown")
        self.assertEqual(before, self.snapshot())

    def test_an_existing_own_completed_session_keeps_its_identity(self):
        own = server.HLS / self.new
        own.mkdir()
        (own / ".done").write_text("already complete")
        self.assertEqual(server._session_dir(self.new), own)


if __name__ == "__main__":
    unittest.main()
