"""Cache browsing must never turn a durable match into a new media read."""
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import server


class CachedPlaybackTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        for name in ("audio", "match", "hls"):
            (self.root / name).mkdir()
        for attr, value in {"AUDIO": self.root / "audio", "MATCH": self.root / "match",
                            "HLS": self.root / "hls", "_jobs": {}}.items():
            p = patch.object(server, attr, value)
            p.start()
            self.addCleanup(p.stop)
        for owner, name in [(server.dubmux, "resolve_url"), (server, "_start_mux"),
                            (server, "_reject_pair"), (server.MEDIA_READS, "register")]:
            p = patch.object(owner, name, side_effect=AssertionError("cache lookup started work"))
            p.start()
            self.addCleanup(p.stop)
        self.dub, self.hq = "cached-dub", "cached-hq"
        self.key = self.dub + "__" + self.hq
        self.match = {"verdict": "accept", "video_check": "ok", "lag": 0,
                      "video_url": "https://offline.invalid/expensive-video"}
        self.write_match()
        self.session = self.root / "hls" / self.key
        self.session.mkdir()
        (self.session / ".done").write_text("complete")
        (self.session / "index.m3u8").write_text(
            "#EXTM3U\n#EXTINF:6,\nseg00000.ts\n#EXTINF:6,\nseg00001.ts\n#EXT-X-ENDLIST\n")
        for i in range(2):
            (self.session / f"seg{i:05d}.ts").write_bytes(b"x" * 188)
        self.layout = self.root / "match" / ("file-" + self.key + ".json")
        self.layout.write_text(json.dumps({"slots": [376, 376]}))

    def write_match(self):
        (self.root / "match" / (self.key + ".json")).write_text(json.dumps(self.match))

    def lookup(self):
        return server.cached_playback(self.dub, self.hq)

    def test_completed_mux_is_read_only_and_does_not_expose_source_url(self):
        before = {p.relative_to(self.root): p.read_bytes()
                  for p in self.root.rglob("*") if p.is_file()}
        self.assertEqual(self.lookup(), {"status": "ready", "cached": True,
                         "result": {"video_check": "ok", "file_ready": True, "file_size": 752}})
        self.assertEqual(before, {p.relative_to(self.root): p.read_bytes()
                                 for p in self.root.rglob("*") if p.is_file()})
        self.assertEqual(server._jobs, {})

    def test_match_and_stale_layout_without_completed_media_are_insufficient(self):
        (self.session / ".done").unlink()
        self.assertEqual(self.lookup()["status"], "unknown")

    def test_missing_or_empty_segment_is_not_cached_playback(self):
        segment = self.session / "seg00001.ts"
        segment.write_bytes(b"")
        self.assertEqual(self.lookup()["status"], "unknown")
        segment.unlink()
        self.assertEqual(self.lookup()["status"], "unknown")
        (self.session / "seg99999.ts").write_bytes(b"x" * 188)
        self.assertEqual(self.lookup()["status"], "unknown")

    def test_failed_unchecked_rejected_and_out_of_sync_are_not_listed(self):
        for change in [{"video_check": None}, {"verdict": "reject:sync"},
                       {"sync_check": {"ok": False}}, {"aligned": "missing.m4a"},
                       {"piecewise": {"verdict": "accept"}}]:
            with self.subTest(change=change):
                original = self.match.copy()
                self.match.update(change)
                self.write_match()
                self.assertEqual(self.lookup()["status"], "unknown")
                self.match = original
        self.write_match()
        (self.session / ".failed").write_text("sync")
        self.assertEqual(self.lookup()["status"], "unknown")

    def test_missing_layout_is_not_recreated_by_browsing(self):
        self.layout.unlink()
        self.assertEqual(self.lookup()["status"], "unknown")
        self.assertFalse(self.layout.exists())

    def test_wrong_slot_count_or_undersized_slot_is_not_playable(self):
        for slots in ([376], [376, 100], [], [-376, 376]):
            with self.subTest(slots=slots):
                self.layout.write_text(json.dumps({"slots": slots}))
                self.assertEqual(self.lookup()["status"], "unknown")


if __name__ == "__main__":
    unittest.main()
