"""Bounded Matroska subtitle reads. Never fall back to a sequential video read.

Uses SeekHead/Cues and preserves native subtitle packets in a small Matroska
artifact; existing local FFmpeg conversion handles ASS, durations and encodings.
The index is advisory: ordinary subtitle quality checks still apply downstream.
"""
from collections import OrderedDict
from concurrent.futures import ThreadPoolExecutor
import re
import threading
import time
import urllib.error
import urllib.request
from urllib.parse import urljoin, urlsplit

MAX_BYTES = 64 * 1024 * 1024
MAX_REQUESTS = 6000
MAX_OUTPUT = 16 * 1024 * 1024
TIMEOUT = 300
PREFETCH = 8192
EBML, SEGMENT, SEEKHEAD = 0x1A45DFA3, 0x18538067, 0x114D9B74
INFO, TRACKS, CUES, CLUSTER = 0x1549A966, 0x1654AE6B, 0x1C53BB6B, 0x1F43B675


class Unavailable(RuntimeError):
    """Only fixed reason codes leave this module; URLs never enter errors."""


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *_args, **_kwargs):
        return None


def vint(data, offset=0, identifier=False):
    if offset >= len(data) or not data[offset]:
        raise Unavailable("invalid_ebml_integer")
    first = data[offset]
    width = 9 - first.bit_length()
    if width > (4 if identifier else 8) or offset + width > len(data):
        raise Unavailable("truncated_ebml_integer")
    value = int.from_bytes(data[offset:offset + width], "big")
    if not identifier:
        value &= (1 << (7 * width)) - 1
        if value == (1 << (7 * width)) - 1:
            value = None
    return value, width


def header(data, offset=0):
    tag, a = vint(data, offset, True)
    size, b = vint(data, offset + a)
    start = offset + a + b
    return tag, start, None if size is None else start + size


def children(data):
    offset = 0
    count = 0
    while offset < len(data):
        count += 1
        if count > 200_000:
            raise Unavailable("too_many_ebml_elements")
        tag, start, end = header(data, offset)
        if end is None or end > len(data) or end <= offset:
            raise Unavailable("truncated_ebml_element")
        yield tag, data[start:end]
        offset = end


def integer(data):
    if not 0 < len(data) <= 8:
        raise Unavailable("invalid_ebml_integer")
    return int.from_bytes(data, "big")


def size_bytes(value):
    for width in range(1, 9):
        if value < (1 << (7 * width)) - 1:
            return (value | (1 << (7 * width))).to_bytes(width, "big")
    raise Unavailable("element_too_large")


def element(tag, payload):
    return tag.to_bytes((tag.bit_length() + 7) // 8, "big") + size_bytes(len(payload)) + payload


def uint_element(tag, value):
    return element(tag, value.to_bytes(max(1, (value.bit_length() + 7) // 8), "big"))


class RangeReader:
    def __init__(self, url, expected_size=0, resolved=None, max_bytes=MAX_BYTES,
                 max_requests=MAX_REQUESTS, timeout=TIMEOUT, request_gap=0.05, reserve=None):
        self.url = url
        self.total = expected_size or None
        self.resolved = resolved
        self.max_bytes, self.max_requests = max_bytes, max_requests
        self.deadline = time.monotonic() + timeout
        self.request_gap = request_gap
        self.next_request = 0
        self.bytes_read = self.requests = self.reserved = 0
        self.etag = None
        self.cache = OrderedDict()
        self.lock = threading.Lock()
        self.cancelled = threading.Event()
        self.opener = urllib.request.build_opener(NoRedirect)
        self.reserve = reserve

    def request_slot(self):
        with self.lock:
            if self.cancelled.is_set():
                raise Unavailable("extraction_cancelled")
            if self.requests >= self.max_requests:
                raise Unavailable("request_limit")
            now = time.monotonic()
            at = max(now, self.next_request)
            if at >= self.deadline:
                raise Unavailable("time_limit")
            self.next_request = at + self.request_gap
            self.requests += 1
        if at > now:
            time.sleep(at - now)

    def open(self, url, headers):
        # urllib's default redirect handler drains redirect bodies. Follow
        # manually, counting every request and closing without reading bodies.
        for _ in range(8):
            self.request_slot()
            remaining = self.deadline - time.monotonic()
            if remaining <= 0:
                raise Unavailable("time_limit")
            try:
                return self.opener.open(urllib.request.Request(url, headers=headers), timeout=min(5, remaining))
            except urllib.error.HTTPError as error:
                if error.code not in (301, 302, 303, 307, 308):
                    error.close()
                    raise Unavailable("range_request_failed") from None
                target = urljoin(url, error.headers.get("Location", ""))
                error.close()
                if target == url or urlsplit(target).scheme not in ("http", "https"):
                    raise Unavailable("invalid_redirect")
                url = target
        raise Unavailable("redirect_limit")

    def read(self, offset, length, prefetch=True):
        if self.cancelled.is_set():
            raise Unavailable("extraction_cancelled")
        if offset < 0 or length < 0 or (self.total is not None and offset + length > self.total):
            raise Unavailable("range_outside_file")
        if not length:
            return b""
        with self.lock:
            for start, data in list(self.cache.items()):
                if start <= offset and offset + length <= start + len(data):
                    self.cache.move_to_end(start)
                    return data[offset - start:offset - start + length]
            count = max(length, PREFETCH if prefetch else length)
            if self.total is not None:
                count = min(count, self.total - offset)
            if count > self.max_bytes - self.reserved:
                raise Unavailable("byte_limit")
            if self.reserve:
                self.reserve(self.reserved + count)
            self.reserved += count
            url, etag = self.url, self.etag
        headers = {"Range": f"bytes={offset}-{offset + count - 1}",
                   "Accept-Encoding": "identity", "User-Agent": "Remux-Subtitle-Range/1"}
        if etag:
            headers["If-Range"] = etag
        try:
            with self.open(url, headers) as response:
                # Refuse ignored Range headers before consuming a media body.
                if response.status != 206:
                    raise Unavailable("range_not_supported")
                match = re.fullmatch(r"bytes (\d+)-(\d+)/(\d+)", response.headers.get("Content-Range", ""))
                if not match:
                    raise Unavailable("invalid_content_range")
                first, last, total = map(int, match.groups())
                if first != offset or total <= last or last != min(offset + count, total) - 1:
                    raise Unavailable("invalid_content_range")
                if response.headers.get("Content-Encoding", "identity") != "identity":
                    raise Unavailable("encoded_range")
                content_length = response.headers.get("Content-Length")
                if content_length is not None and int(content_length) != last - first + 1:
                    raise Unavailable("invalid_content_length")
                final_url = response.geturl()
                if self.resolved:
                    self.resolved(final_url)
                with self.lock:
                    if self.total is not None and self.total != total:
                        raise Unavailable("source_size_changed")
                    current_etag = response.headers.get("ETag")
                    if self.etag and current_etag != self.etag:
                        raise Unavailable("source_changed")
                    self.total, self.url = total, final_url
                    if current_etag and not current_etag.startswith("W/"):
                        self.etag = current_etag
                data = bytearray()
                expected = last - first + 1
                while len(data) < expected:
                    if time.monotonic() >= self.deadline:
                        raise Unavailable("time_limit")
                    chunk = response.read1(min(64 * 1024, expected - len(data)))
                    if not chunk:
                        raise Unavailable("short_range")
                    data.extend(chunk)
                    with self.lock:
                        self.bytes_read += len(chunk)
                data = bytes(data)
        except (OSError, ValueError, urllib.error.HTTPError):
            raise Unavailable("range_request_failed") from None
        with self.lock:
            self.cache[offset] = data
            while len(self.cache) > 128:
                self.cache.popitem(last=False)
        if len(data) < length:
            raise Unavailable("short_range")
        return data[:length]

    def remote_header(self, offset):
        data = self.read(offset, min(16, self.total - offset) if self.total else 16)
        tag, start, end = header(data)
        return tag, offset + start, None if end is None else offset + end

    def remote_element(self, offset, maximum=8 * 1024 * 1024):
        tag, start, end = self.remote_header(offset)
        if end is None or end - offset > maximum:
            raise Unavailable("metadata_element_too_large")
        return tag, self.read(start, end - start), self.read(offset, end - offset)


def extract(reader, subtitle_ordinals):
    if not subtitle_ordinals or len(subtitle_ordinals) > 8 or any(type(n) is not int or not 0 <= n <= 1024 for n in subtitle_ordinals):
        raise Unavailable("invalid_track_selection")
    tag, start, end = reader.remote_header(0)
    if tag != EBML or end is None or end > 4096:
        raise Unavailable("not_matroska")
    ebml = reader.read(0, end)
    offset = end
    for _ in range(16):
        tag, start, end = reader.remote_header(offset)
        if tag == SEGMENT:
            break
        if tag != 0xEC or end is None:  # legal leading Void only
            raise Unavailable("missing_segment")
        offset = end
    else:
        raise Unavailable("missing_segment")
    segment = start
    metadata, locations = {}, {}
    visited = set()
    offset = segment
    for _ in range(256):
        if all(k in metadata for k in (INFO, TRACKS, CUES)):
            break
        candidates = [p for k, p in locations.items() if k not in metadata and p not in visited]
        if candidates:
            position = candidates[0]
        else:
            position = offset
        if position in visited or position >= reader.total:
            raise Unavailable("missing_subtitle_index")
        visited.add(position)
        tag, payload_start, payload_end = reader.remote_header(position)
        if tag in (SEEKHEAD, INFO, TRACKS, CUES):
            _, payload, raw = reader.remote_element(position)
            if tag == SEEKHEAD:
                for child, entry in children(payload):
                    if child != 0x4DBB:
                        continue
                    fields = dict(children(entry))
                    if 0x53AB in fields and 0x53AC in fields:
                        target = integer(fields[0x53AB])
                        if target in (INFO, TRACKS, CUES):
                            locations[target] = segment + integer(fields[0x53AC])
            else:
                metadata[tag] = (payload, raw)
        if position == offset:
            if payload_end is None:
                raise Unavailable("missing_subtitle_index")
            offset = payload_end
    if not all(k in metadata for k in (INFO, TRACKS, CUES)):
        raise Unavailable("missing_subtitle_index")
    selected, ordinal = set(), 0
    for tag, payload in children(metadata[TRACKS][0]):
        if tag != 0xAE:
            continue
        fields = dict(children(payload))
        if integer(fields.get(0x83, b"\x00")) != 17:
            continue
        if ordinal in subtitle_ordinals:
            if fields.get(0x86) not in (b"S_TEXT/UTF8", b"S_TEXT/ASS", b"S_TEXT/SSA", b"S_TEXT/WEBVTT"):
                raise Unavailable("unsupported_subtitle_codec")
            if any(k in fields for k in (0x537F, 0x23314F)):  # non-default track clocks
                raise Unavailable("unsupported_track_clock")
            selected.add(integer(fields[0xD7]))
        ordinal += 1
    if len(selected) != len(set(subtitle_ordinals)):
        raise Unavailable("missing_selected_track")
    points, indexed = [], set()
    for tag, payload in children(metadata[CUES][0]):
        if tag != 0xBB:
            continue
        fields = list(children(payload))
        times = [integer(v) for k, v in fields if k == 0xB3]
        if len(times) != 1:
            raise Unavailable("invalid_subtitle_index")
        for k, v in fields:
            if k != 0xB7:
                continue
            f = dict(children(v))
            track = integer(f.get(0xF7, b"\x00"))
            if track not in selected:
                continue
            if any(k not in f for k in (0xF1, 0xF0)) or integer(f.get(0xEA, b"\x00")):
                raise Unavailable("incomplete_subtitle_index")
            location = (segment + integer(f[0xF1]), integer(f[0xF0]))
            if location in indexed:
                raise Unavailable("duplicate_subtitle_index")
            indexed.add(location)
            points.append((times[0], track, *location, integer(f[0xB2]) if 0xB2 in f else None))
    if not points or len(points) > 20_000 or selected != {p[1] for p in points}:
        raise Unavailable("incomplete_subtitle_index")
    output = bytearray(ebml + b"\x18\x53\x80\x67\x01\xff\xff\xff\xff\xff\xff\xff"
                       + metadata[INFO][1] + metadata[TRACKS][1])

    def read_block(point):
        timestamp, track, cluster, relative, cue_duration = point
        # Tiny header read, rather than prefetching the first video frame.
        raw = reader.read(cluster, 16, prefetch=False)
        tag, cluster_data, _ = header(raw)
        if tag != CLUSTER:
            raise Unavailable("invalid_cluster_index")
        tag, payload, _ = reader.remote_element(cluster + cluster_data + relative, 1024 * 1024)
        if tag == 0xA0:
            entries = list(children(payload))
            blocks = [v for k, v in entries if k == 0xA1]
            if len(blocks) != 1 or any(k in (0xFB, 0x75A2) for k, _ in entries):
                raise Unavailable("unsupported_subtitle_block")
            data = blocks[0]
        elif tag == 0xA3 and cue_duration is not None:
            data, entries = payload, None
        else:
            raise Unavailable("unsupported_subtitle_block")
        number, width = vint(data)
        if number != track or len(data) < width + 3 or data[width + 2] & 6:
            raise Unavailable("invalid_subtitle_block")
        flags = data[width + 2] & 8 if entries is None else data[width + 2]
        normalized = data[:width] + b"\x00\x00" + bytes([flags]) + data[width + 3:]
        if entries is None:
            body = element(0xA1, normalized) + uint_element(0x9B, cue_duration)
        else:
            if not any(k == 0x9B for k, _ in entries) and cue_duration is None:
                raise Unavailable("missing_subtitle_duration")
            body = b"".join(element(k, normalized if k == 0xA1 else v) for k, v in entries if k != 0xBF)
            if not any(k == 0x9B for k, _ in entries):
                body += uint_element(0x9B, cue_duration)
        return element(CLUSTER, uint_element(0xE7, timestamp) + element(0xA0, body))

    def block(point):
        try:
            return read_block(point)
        except Exception:
            reader.cancelled.set()
            raise

    # Four connections per source, with shared byte/request/time accounting.
    with ThreadPoolExecutor(max_workers=4) as pool:
        for packet in pool.map(block, sorted(points)):
            output.extend(packet)
            if len(output) > MAX_OUTPUT:
                reader.cancelled.set()
                raise Unavailable("output_limit")
    return bytes(output)
