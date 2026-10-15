"""Synthetic Matroska + loopback HTTP only; no debrid or real media links."""
import contextlib
import http.server
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import threading
import unittest

import indexed_subtitles as ix


def fixture(indexed=True, relative=True, wrong_track=False, ass=False):
    e, u = ix.element, ix.uint_element
    ebml = e(ix.EBML, u(0x4286, 1) + u(0x42F7, 1) + u(0x42F2, 4) + u(0x42F3, 8)
             + e(0x4282, b"matroska") + u(0x4287, 4) + u(0x4285, 2))
    info = e(ix.INFO, u(0x2AD7B1, 1_000_000))
    private = b"[Script Info]\nScriptType: v4.00+\n[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\nStyle: Default,Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,1,0,2,10,10,10,1\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n"
    tracks = e(ix.TRACKS,
        e(0xAE, u(0xD7, 1) + u(0x73C5, 1) + u(0x83, 1) + e(0x86, b"V_VP8") + e(0xE0, u(0xB0, 64) + u(0xBA, 64)))
        + e(0xAE, u(0xD7, 9) + u(0x73C5, 9) + u(0x83, 17) + e(0x86, b"S_TEXT/UTF8") + e(0x22B59C, b"eng"))
        + e(0xAE, u(0xD7, 17) + u(0x73C5, 17) + u(0x83, 17) + e(0x86, b"S_TEXT/ASS" if ass else b"S_TEXT/UTF8") + (e(0x63A2, private) if ass else b"") + e(0x22B59C, b"vie")))
    def seek(positions):
        return e(ix.SEEKHEAD, b"".join(e(0x4DBB, e(0x53AB, k.to_bytes(4, 'big'))
            + e(0x53AC, v.to_bytes(8, 'big'))) for k, v in positions.items()))
    seekhead = seek({ix.INFO: 0, ix.TRACKS: 0, ix.CUES: 0})
    info_at, tracks_at = len(seekhead), len(seekhead) + len(info)
    position = tracks_at + len(tracks)
    clusters, cues = [], []
    for n, timestamp in enumerate((1000, 4000, 7000)):
        video = e(0xA3, b"\x81\x00\x00\x80" + b"v" * (2 * 1024 * 1024))
        clock = u(0xE7, timestamp)
        en = e(0xA0, e(0xA1, b"\x89\x00\x64\x00English fixture") + u(0x9B, 1200))
        text = ((f"{n},0,Default,,0,0,0,," if ass else "") + f"Lời thoại tiếng Việt {n + 1}").encode()
        vi = e(0xA0, e(0xA1, (b"\x89" if wrong_track else b"\x91") + b"\x00\xc8\x00" + text) + u(0x9B, 1300))
        offset = len(clock) + len(video) + len(en)
        cluster = e(ix.CLUSTER, clock + video + en + vi)
        cues.append(e(0xBB, u(0xB3, timestamp + 200) + e(0xB7, u(0xF7, 17) + u(0xF1, position)
                     + (u(0xF0, offset) if relative else b"") + u(0xB2, 1300))))
        clusters.append(cluster)
        position += len(cluster)
    index = e(ix.CUES, b"".join(cues)) if indexed else b""
    seekhead = seek({ix.INFO: info_at, ix.TRACKS: tracks_at, ix.CUES: position}) if indexed else seek({ix.INFO: info_at, ix.TRACKS: tracks_at, ix.CUES: position})
    body = seekhead + info + tracks + b"".join(clusters) + index
    return ebml + e(ix.SEGMENT, body)


@contextlib.contextmanager
def source(data, mode="range"):
    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass
        def do_GET(self):
            self.server.requests += 1
            if mode == 'redirect' and self.path == '/fixture.mkv':
                self.send_response(302)
                self.send_header('Location','/actual.mkv')
                self.send_header('Content-Length','1000000000')
                self.end_headers()
                return
            match = re.fullmatch(r"bytes=(\d+)-(\d+)", self.headers.get("Range", ""))
            if mode == "ignore":
                self.send_response(200)
                self.send_header('Content-Length', str(len(data)))
                self.end_headers()
                return
            if not match:
                self.send_error(400)
                return
            a, b = map(int, match.groups())
            b = min(b, len(data) - 1)
            self.send_response(206)
            self.send_header('Content-Length', str(b - a + 1))
            self.send_header('Content-Range', f'bytes {a}-{b}/{len(data)}')
            self.send_header('ETag', '"fixture"' if mode != "changed" or self.server.requests == 1 else '"changed"')
            self.end_headers()
            try:
                self.wfile.write(data[a:b + 1])
            except (BrokenPipeError, ConnectionResetError):
                pass
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    server.requests = 0
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f'http://127.0.0.1:{server.server_port}/fixture.mkv', server
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


class IndexedSubtitlesTest(unittest.TestCase):
    @unittest.skipUnless(shutil.which('ffmpeg'), 'ffmpeg unavailable')
    def test_range_extraction_preserves_track_ordinal_text_and_timestamps(self):
        data = fixture()
        with source(data) as (url, _):
            reader = ix.RangeReader(url, len(data), request_gap=0)
            artifact = ix.extract(reader, [1])
            self.assertLess(reader.bytes_read, len(data) // 20)
            self.assertLess(reader.requests, 30)
            self.assertNotIn(b'v' * 100_000, artifact)
        with tempfile.TemporaryDirectory() as directory:
            full = Path(directory) / 'full.mkv'
            fast = Path(directory) / 'fast.mks'
            full.write_bytes(data)
            fast.write_bytes(artifact)
            def text(path):
                return subprocess.check_output(['ffmpeg','-nostdin','-v','error','-copyts','-i',str(path),
                    '-map','0:s:1','-c:s','srt','-f','srt','-'],stderr=subprocess.DEVNULL,timeout=15)
            expected, actual = text(full), text(fast)
            self.assertEqual(expected, actual)
            self.assertIn('Lời thoại tiếng Việt 3'.encode(), actual)
            self.assertIn(b'00:00:01,200 --> 00:00:02,500', actual)
            self.assertNotIn(b'English', actual)
        print(f'Indexed fixture: {reader.bytes_read}/{len(data)} response bytes, {reader.requests} ranges; identical SRT')

    @unittest.skipUnless(shutil.which('ffmpeg'), 'ffmpeg unavailable')
    def test_ass_styles_and_plain_conversion_preserve_original_timing(self):
        data=fixture(ass=True)
        with source(data) as (url,_):
            artifact=ix.extract(ix.RangeReader(url,len(data),request_gap=0),[1])
        with tempfile.TemporaryDirectory() as directory:
            full,fast=Path(directory)/'full.mkv',Path(directory)/'fast.mks'
            full.write_bytes(data)
            fast.write_bytes(artifact)
            for codec in ('ass','srt'):
                def convert(path):
                    return subprocess.check_output(['ffmpeg','-nostdin','-v','error','-copyts','-i',str(path),
                        '-map','0:s:1','-c:s','copy' if codec=='ass' else 'srt','-f',codec,'-'],
                        stderr=subprocess.DEVNULL,timeout=15)
                self.assertEqual(convert(full),convert(fast))
                self.assertIn('Lời thoại tiếng Việt'.encode(),convert(fast))

    def test_missing_or_incomplete_index_never_causes_a_full_read(self):
        for data in (fixture(indexed=False), fixture(relative=False), fixture(wrong_track=True)):
            with source(data) as (url, _):
                reader = ix.RangeReader(url, len(data), request_gap=0)
                with self.assertRaises(ix.Unavailable):
                    ix.extract(reader, [1])
                self.assertLess(reader.bytes_read, 100_000)

    def test_ignored_ranges_are_refused_before_body_consumption(self):
        with source(fixture(), 'ignore') as (url, _):
            reader = ix.RangeReader(url, request_gap=0)
            with self.assertRaisesRegex(ix.Unavailable, 'range_not_supported'):
                ix.extract(reader, [1])
            self.assertEqual(reader.bytes_read, 0)
            self.assertEqual(reader.requests, 1)

    def test_limits_and_changed_sources_fail_closed(self):
        data = fixture()
        with source(data) as (url, _):
            with self.assertRaisesRegex(ix.Unavailable, 'byte_limit'):
                ix.extract(ix.RangeReader(url, len(data), max_bytes=16, request_gap=0), [1])
            with self.assertRaisesRegex(ix.Unavailable, 'request_limit'):
                ix.extract(ix.RangeReader(url, len(data), max_requests=1, request_gap=0), [1])
            with self.assertRaisesRegex(ix.Unavailable, 'time_limit'):
                ix.extract(ix.RangeReader(url, len(data), timeout=0, request_gap=0), [1])
        with source(data, 'changed') as (url, _):
            with self.assertRaisesRegex(ix.Unavailable, 'source_changed'):
                ix.extract(ix.RangeReader(url, len(data), request_gap=0), [1])

    def test_unknown_size_can_be_discovered_with_a_bounded_range(self):
        data = fixture()
        with source(data) as (url, _):
            reader = ix.RangeReader(url, request_gap=0)
            self.assertTrue(ix.extract(reader, [1]).startswith(b'\x1a\x45\xdf\xa3'))
            self.assertEqual(reader.total, len(data))

    def test_redirects_are_counted_without_draining_redirect_bodies(self):
        data=fixture()
        with source(data,'redirect') as (url,server):
            reader=ix.RangeReader(url,len(data),request_gap=0)
            self.assertTrue(ix.extract(reader,[1]).startswith(b'\x1a\x45\xdf\xa3'))
            self.assertEqual(reader.requests,server.requests)
            self.assertLess(reader.bytes_read,100_000)
        with source(data,'redirect') as (url,server):
            with self.assertRaisesRegex(ix.Unavailable,'request_limit'):
                ix.extract(ix.RangeReader(url,len(data),request_gap=0,max_requests=1),[1])
            self.assertEqual(server.requests,1)


if __name__ == '__main__':
    unittest.main()
