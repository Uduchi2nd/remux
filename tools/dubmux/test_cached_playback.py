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

    def test_completed_video_stays_available_when_paid_work_is_blocked(self):
        with patch.object(server.MEDIA_READS, "check", side_effect=AssertionError("cached media checked admission")):
            result = server.status(self.dub, self.hq)["result"]
        self.assertTrue(result["playback_available"])
        self.assertEqual(result["video_check"], "ok")

    def test_evicted_video_does_not_claim_playability_when_admission_denies_it(self):
        (self.session / ".done").unlink()
        before = (server.MATCH / (self.key + ".json")).read_bytes()
        blocked = server.provider_budget.Limited("daily_reservation_budget", 120)
        with patch.object(server.MEDIA_READS, "check", side_effect=blocked) as check:
            result = server.status(self.dub, self.hq)["result"]
        check.assert_called_once_with(self.hq, self.match["video_url"])
        self.assertFalse(result["playback_available"])
        self.assertEqual(result["playback_deferred"], "daily_reservation_budget")
        self.assertEqual(result["video_check"], "ok", "admission is separate from decode proof")
        self.assertTrue(result["file_ready"], "an old layout is not completed media")
        self.assertEqual(before, (server.MATCH / (self.key + ".json")).read_bytes())
        self.assertFalse((self.session / ".done").exists())

    def test_evicted_video_can_recover_with_admission_and_current_url(self):
        (self.session / ".done").unlink()
        current_url = "https://offline.invalid/refreshed-video"
        with patch.object(server.MEDIA_READS, "check") as check:
            result = server._playback_result(self.key, self.match, current_url)
        check.assert_called_once_with(self.hq, current_url)
        self.assertTrue(result["playback_available"])

    def test_completed_raw_video_needs_no_paid_read(self):
        (self.session / ".done").unlink()
        raw = server._raw_dir(self.hq)
        raw.mkdir(exist_ok=True)
        (raw / ".done").write_text("complete")
        with patch.object(server.MEDIA_READS, "check", side_effect=AssertionError("raw cache checked admission")):
            result = server.status(self.dub, self.hq)["result"]
        self.assertTrue(result["playback_available"])

    def test_wrong_slot_count_or_undersized_slot_is_not_playable(self):
        for slots in ([376], [376, 100], [], [-376, 376]):
            with self.subTest(slots=slots):
                self.layout.write_text(json.dumps({"slots": slots}))
                self.assertEqual(self.lookup()["status"], "unknown")


if __name__ == "__main__":
    unittest.main()
