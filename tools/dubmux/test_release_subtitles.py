"""Offline subtitle preservation tests. No provider URLs or debrid traffic."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time
import unittest
import uuid
from unittest.mock import patch

import release_subtitles as subs


class ReleaseSubtitlesTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.match = self.root / "match"
        self.match.mkdir()
        self.cache = self.root / "cache"
        self.staging = self.root / "staging"
        self.identity = str(uuid.uuid4())
        self.video = {"id": "release", "subtitle_source": {
            "identity": self.identity,
            "tracks": [{"index": 2, "codec": "ass"}, {"index": 3, "codec": "subrip"},
                       {"index": 4, "codec": "hdmv_pgs_subtitle"}]}}

    def plan(self):
        subs.register(self.match, self.video)
        return subs.outputs(self.match, "release", self.staging)

    def test_only_text_tracks_and_safe_indices_are_mapped(self):
        identity, args = self.plan()
        self.assertEqual(identity, self.identity)
        self.assertNotIn("-i", args)
        self.assertEqual([args[i+1] for i, x in enumerate(args) if x == "-map"], ["0:2", "0:3"])
        self.assertIsNone(subs.checked_plan({"identity": "../../other", "tracks": []}))

    def test_miss_and_partial_outputs_do_not_read_or_publish_video(self):
        identity, _ = self.plan()
        (self.staging / "2.srt").write_text("1\n00:00:01,000 --> 00:00:02,000\npartial\n")
        self.assertIsNone(subs.cached_path(self.cache, identity, 2))
        self.assertEqual(subs.outputs(self.match, "missing", self.root / "absent"), (None, []))
        self.assertFalse((self.root / "absent").exists())

    def test_empty_and_oversized_files_are_not_published(self):
        identity, _ = self.plan()
        (self.staging / "2.srt").write_text("")
        (self.staging / "3.srt").write_bytes(b"x" * (subs.MAX_BYTES + 1))
        subs.publish(self.cache, identity, self.staging)
        self.assertIsNone(subs.cached_path(self.cache, identity, 2))
        self.assertIsNone(subs.cached_path(self.cache, identity, 3))

    def test_identity_expiry_and_shared_budget_are_enforced(self):
        identity, _ = self.plan()
        text = "1\n00:00:01,000 --> 00:00:02,000\nReal subtitle text\n"
        (self.staging / "2.srt").write_text(text)
        subs.publish(self.cache, identity, self.staging)
        p = subs.cached_path(self.cache, identity, 2)
        self.assertIsNotNone(p)
        self.assertIsNone(subs.cached_path(self.cache, str(uuid.uuid4()), 2))
        self.assertIsNone(subs.cached_path(self.cache, identity, 2, time.time() + subs.TTL_SECONDS + 1))
        os.utime(p, (time.time() - 10, time.time() - 10))
        (self.staging / "3.srt").write_text(text)
        with patch.object(subs, "MAX_FILES", 1):
            subs.publish(self.cache, identity, self.staging)
        self.assertEqual(len(list(self.cache.glob("*/*.srt"))), 1)
        with patch.object(subs, "BUDGET_BYTES", 1):
            subs.sweep(self.cache)
        self.assertEqual(list(self.cache.glob("*/*.srt")), [])

    @unittest.skipUnless(shutil.which("ffmpeg"), "ffmpeg unavailable")
    def test_one_input_preserves_both_release_tracks_with_original_timing(self):
        english = self.root / "en.srt"
        vietnamese = self.root / "vi.srt"
        english.write_text("1\n00:00:00,500 --> 00:00:01,500\nEnglish release dialogue\n\n2\n00:00:03,000 --> 00:00:04,500\nEnglish ending\n")
        vietnamese.write_text("1\n00:00:00,500 --> 00:00:01,500\nLời thoại tiếng Việt\n\n2\n00:00:03,000 --> 00:00:04,500\nKết thúc tiếng Việt\n")
        video = self.root / "original.mkv"
        def run(args):
            subprocess.run(["ffmpeg", "-nostdin", "-y", "-v", "error", *args], check=True,
                           capture_output=True, timeout=30)
        run(["-f", "lavfi", "-i", "color=size=64x64:rate=10:duration=6", "-f", "lavfi", "-i", "sine=duration=6",
             "-i", str(english), "-i", str(vietnamese), "-map", "0:v", "-map", "1:a", "-map", "2:s", "-map", "3:s",
             "-c:v", "libx264", "-c:a", "aac", "-c:s", "ass", "-metadata:s:s:0", "language=eng",
             "-metadata:s:s:1", "language=vie", str(video)])
        identity, outputs = self.plan()
        cmd = ["-i", str(video), "-map", "0:v:0", "-map", "0:a?", "-sn", "-c", "copy",
               "-f", "hls", "-hls_time", "2", "-hls_playlist_type", "vod", str(self.root / "index.m3u8"), *outputs]
        self.assertEqual(cmd.count("-i"), 1)
        run(cmd)
        expected = self.root / "original-vietnamese.srt"
        run(["-i", str(video), "-map", "0:3", "-c:s", "srt", str(expected)])
        # Only the video caller's successful completion publishes these.
        subs.finish(self.cache, identity, self.staging)
        video.unlink()
        body = subs.cached_path(self.cache, identity, 3).read_text()
        self.assertIn("Lời thoại tiếng Việt", body)
        # AAC priming differs across ffmpeg versions; compare the actual
        # source's clock rather than assuming a fixed encoder delay.
        self.assertEqual(body, expected.read_text())
        self.assertIn("English release dialogue", subs.cached_path(self.cache, identity, 2).read_text())

    def test_bad_local_subtitle_conversion_never_reopens_the_video(self):
        identity, _ = self.plan()
        (self.staging / "embedded.mks").write_bytes(b"bad text container")
        with patch.object(subs.subprocess, "run", return_value=subprocess.CompletedProcess([], 1)) as call:
            subs.finish(self.cache, identity, self.staging)
        self.assertEqual(call.call_count, 2)
        for args, kwargs in call.call_args_list:
            self.assertEqual(args[0][args[0].index("-i")+1], str(self.staging / "embedded.mks"))
        self.assertEqual(list(self.cache.glob("*/*.srt")), [])


if __name__ == "__main__":
    unittest.main()
