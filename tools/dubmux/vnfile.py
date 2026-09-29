"""vnphim playlists as seekable virtual files (2026-09-28).

vnphim adds, for viewers outside VN, one extra stream per episode whose URL is
`{public}/vn/<id>/file.ts` on this seedbox. `<id>` is an opaque random id
vnphim registered; `/_internal/file-source?id=` (keyed) tells us the playlist
behind it — vnphim's own ad-stripped playlist with ORIGIN segment URLs
(`via=mf`) — and whether its segments must be fetched in VN (kkphim/ophim:
geo-blocked abroad, fetched by the VN extractor over the tailnet) or can be
fetched from here (yanhh3d CDN, hotphim tiktokcdn byte ranges).

The playlist becomes ONE virtual MPEG-TS file, like the dub muxer's
`file.ts`: every segment owns a fixed-size slot (its exact size when bytes
are passed through; +5 % +64 KB when timestamps must be rewritten), the
total size is known before anything is fetched, a byte range maps to
segments arithmetically, and the slot tail is MPEG-TS null packets.
Segments are fetched on demand and PREFETCHED ahead in parallel (the VN
relay does ~370 KB/s per segment; 1080p needs ~250 KB/s), cached on disk
for a few days.

Ad-stripped kkphim playlists jump in time where ads were cut
(#EXT-X-DISCONTINUITY); a plain file cannot signal that, so such playlists
are REBASED: every segment is re-muxed (stream copy) with its timestamps
shifted onto the playlist's own timeline (sum of EXTINF durations).
"""
import json, os, re, subprocess, sys, threading, time, urllib.request
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from urllib.parse import urljoin

import dubmux

UA = "Mozilla/5.0"
AHEAD = int(os.environ.get("VNFILE_AHEAD", "12"))            # segments prefetched past a request
URGENT = ThreadPoolExecutor(max_workers=int(os.environ.get("VNFILE_URGENT", "6")))
PREFETCH = ThreadPoolExecutor(max_workers=int(os.environ.get("VNFILE_PREFETCH", "10")))
SIZE_POOL = ThreadPoolExecutor(max_workers=32)
KEEP_DAYS = float(os.environ.get("VNFILE_KEEP_DAYS", "3"))
REBASE_FACTOR, REBASE_MARGIN = 1.05, 65536

_lock = threading.Lock()
_inflight: dict = {}          # (id, i) -> Future
_tables: dict = {}            # id -> table (memory copy)


def _dirs(hls, match, fid):
    return hls / f"vn-{fid}", match / f"vn-{fid}.json"


def _http(url, hdr=None, timeout=60):
    req = urllib.request.Request(url, headers={"User-Agent": UA, **(hdr or {})})
    return urllib.request.urlopen(req, timeout=timeout)


def resolve(fid):
    """vnphim: {"playlist": url, "vn": bool} for a registered id, or None."""
    if not (dubmux.VNPHIM_URL and dubmux.VNPHIM_KEY):
        return None
    try:
        with _http(f"{dubmux.VNPHIM_URL}/_internal/file-source?id={fid}",
                   {"X-Vnphim-Key": dubmux.VNPHIM_KEY}, timeout=30) as r:
            return json.loads(r.read())
    except Exception as e:  # noqa: BLE001
        print(f"vnfile {fid}: file-source lookup failed: {e}", file=sys.stderr, flush=True)
        return None


def parse(url):
    """Media playlist -> [{"url","dur","br":[off,len]|None,"disc"}]; follows a
    master to its first variant. Raises on encrypted / fMP4 playlists."""
    with _http(url) as r:
        text = r.read().decode("utf8", "replace")
    if "#EXT-X-STREAM-INF" in text:
        if "#EXT-X-MEDIA:TYPE=AUDIO" in text and "URI=" in text:
            raise ValueError("demuxed audio rendition (not a single stream)")
        variant = next(l for l in text.splitlines() if l and not l.startswith("#"))
        return parse(urljoin(url, variant.strip()))
    if "#EXT-X-KEY" in text and "METHOD=NONE" not in text:
        raise ValueError("encrypted playlist")
    if "#EXT-X-MAP" in text:
        raise ValueError("fMP4 segments")
    segs, dur, br, disc, next_off = [], None, None, False, 0
    for line in text.splitlines():
        line = line.strip()
        if line.startswith("#EXTINF:"):
            dur = float(line[8:].split(",")[0])
        elif line.startswith("#EXT-X-BYTERANGE:"):
            spec = line[17:]
            n, _, o = spec.partition("@")
            off = int(o) if o else next_off
            br = [off, int(n)]
            next_off = off + int(n)
        elif line.startswith("#EXT-X-DISCONTINUITY") and not line.startswith("#EXT-X-DISCONTINUITY-SEQUENCE"):
            disc = True
        elif line and not line.startswith("#"):
            segs.append({"url": urljoin(url, line), "dur": dur or 0.0, "br": br, "disc": disc})
            dur, br, disc = None, None, False
    if not segs:
        raise ValueError("no segments")
    return segs


def _direct_size(url):
    for attempt in range(3):
        try:
            with _http(url, {"Range": "bytes=0-0"}, timeout=20) as r:
                cr = r.headers.get("Content-Range", "")
                if "/" in cr and cr.rsplit("/", 1)[1].isdigit():
                    return int(cr.rsplit("/", 1)[1])
                cl = r.headers.get("Content-Length")
                if r.status == 200 and cl and cl.isdigit():
                    return int(cl)
        except Exception:  # noqa: BLE001
            time.sleep(0.5 * (attempt + 1))
    return None


def build_table(hls, match, fid):
    """Resolve, parse, size and lay out; saved once (a file's size never
    changes under a player). Returns the table or None."""
    d, tp = _dirs(hls, match, fid)
    if fid in _tables:
        return _tables[fid]
    if tp.exists():
        try:
            t = json.load(open(tp))
            _tables[fid] = t
            return t
        except Exception:  # noqa: BLE001
            pass
    with _BUILD_SLOTS:          # warm-ups for many episodes must not pile up
        if fid in _tables:
            return _tables[fid]
        return _build(hls, match, fid, d, tp)


_BUILD_SLOTS = threading.BoundedSemaphore(int(os.environ.get("VNFILE_BUILDS", "2")))


def _build(hls, match, fid, d, tp):
    src = resolve(fid)
    if not src or not src.get("playlist"):
        return None
    segs = parse(src["playlist"])
    vn = bool(src.get("vn"))
    sizes = [s["br"][1] if s["br"] else None for s in segs]
    missing = [i for i, x in enumerate(sizes) if x is None]
    if missing:
        urls = [segs[i]["url"] for i in missing]
        got = dubmux.vn_sizes(urls) if vn else list(SIZE_POOL.map(_direct_size, urls))
        for i, x in zip(missing, got):
            sizes[i] = x
    if any(x is None for x in sizes):
        raise RuntimeError(f"{sizes.count(None)} segment sizes unknown")
    rebase = any(s["disc"] for s in segs[1:])
    if rebase:
        slots = [int(x * REBASE_FACTOR) + REBASE_MARGIN for x in sizes]
    else:
        slots = list(sizes)
    slots = [(x + 187) // 188 * 188 for x in slots]
    starts, t = [], 0.0
    for s in segs:
        starts.append(round(t, 6))
        t += s["dur"]
    table = {"id": fid, "vn": vn, "rebase": rebase, "segs": segs, "sizes": sizes, "slots": slots,
             "starts": starts, "duration": round(t, 3), "created": int(time.time()),
             "source": src["playlist"].split("?")[0][:120]}
    json.dump(table, open(tp, "w"))
    d.mkdir(parents=True, exist_ok=True)
    _tables[fid] = table
    # players read the start and the END (duration) first: have them ready
    for i in sorted({0, 1, 2, len(segs) - 1}):
        _submit(hls, match, fid, i, False)
    print(f"vnfile {fid}: {len(segs)} segments, {sum(slots) / 1e6:.0f} MB, vn={vn} rebase={rebase}",
          file=sys.stderr, flush=True)
    return table


def _first_pts(path):
    out = subprocess.run(["ffprobe", "-v", "error", "-show_entries", "packet=pts_time", "-read_intervals",
                          "%+#8", "-of", "csv=p=0", str(path)], capture_output=True, timeout=60).stdout
    vals = []
    for x in out.decode().split():
        try:
            vals.append(float(x.strip(",")))
        except ValueError:
            pass
    return min(vals) if vals else None


def _runs(table):
    """Index of the first segment of each segment's discontinuity run."""
    if "run_start" not in table:
        rs, a = [], 0
        for i, s in enumerate(table["segs"]):
            if i and s["disc"]:
                a = i
            rs.append(a)
        table["run_start"] = rs
    return table["run_start"]


_anchor_locks: dict = {}


def _anchor_shift(hls, match, fid, a):
    """Timestamp shift of run starting at segment a: its first segment's
    original first PTS moved to the playlist timeline (+10 s keeps every
    timestamp positive). One shift per run keeps the run's own spacing."""
    d, _ = _dirs(hls, match, fid)
    table = _tables[fid]
    f = d / "anchors.json"
    with _lock:
        lk = _anchor_locks.setdefault((fid, a), threading.Lock())
    with lk:
        try:
            anchors = json.load(open(f))
        except Exception:  # noqa: BLE001
            anchors = {}
        if str(a) in anchors:
            return anchors[str(a)]
        raw = d / f".a{a:05d}.raw"
        if not raw.exists():
            raw.write_bytes(_download(table, a))
        pts = _first_pts(raw)
        raw.unlink(missing_ok=True)
        shift = (table["starts"][a] + 10.0) - pts if pts is not None else 0.0
        with _lock:
            try:
                anchors = json.load(open(f))
            except Exception:  # noqa: BLE001
                anchors = {}
            anchors[str(a)] = shift
            json.dump(anchors, open(f, "w"))
        return shift


def _download(table, i):
    s = table["segs"][i]
    if table["vn"]:
        data = dubmux.vn_segment(s["url"])
    else:
        hdr = {"Range": f"bytes={s['br'][0]}-{s['br'][0] + s['br'][1] - 1}"} if s["br"] else {}
        data = None
        for attempt in range(4):
            try:
                with _http(s["url"], hdr, timeout=60) as r:
                    data = r.read()
                break
            except Exception:  # noqa: BLE001
                if attempt == 3:
                    raise
                time.sleep(0.5 * (attempt + 1))
    if s["br"] and len(data) != s["br"][1]:
        raise IOError(f"short range read {len(data)} != {s['br'][1]}")
    return data


def _fetch(hls, match, fid, i):
    table = _tables[fid]
    d, _ = _dirs(hls, match, fid)
    d.mkdir(parents=True, exist_ok=True)
    out = d / f"s{i:05d}.ts"
    if out.exists():
        return out
    tmp = d / f".s{i:05d}.part"
    tmp.write_bytes(_download(table, i))
    if table["rebase"]:
        shift = _anchor_shift(hls, match, fid, _runs(table)[i])
        rb = d / f".s{i:05d}.rb"
        r = subprocess.run(["ffmpeg", "-nostdin", "-y", "-v", "error", "-copyts", "-i", str(tmp),
                            "-map", "0", "-c", "copy", "-muxdelay", "0", "-muxpreload", "0",
                            "-output_ts_offset", f"{shift:.6f}", "-f", "mpegts", str(rb)],
                           capture_output=True, timeout=120)
        if r.returncode == 0 and rb.exists():
            os.replace(rb, tmp)
        else:
            rb.unlink(missing_ok=True)
            print(f"vnfile {fid}: rebase of segment {i} failed: {r.stderr.decode()[-200:]}",
                  file=sys.stderr, flush=True)
    os.replace(tmp, out)
    return out


def _submit(hls, match, fid, i, urgent):
    key = (fid, i)
    with _lock:
        f = _inflight.get(key)
        if f and not f.done():
            return f
        pool = URGENT if urgent else PREFETCH
        f = pool.submit(_fetch, hls, match, fid, i)
        _inflight[key] = f
        if len(_inflight) > 5000:
            for k in [k for k, v in _inflight.items() if v.done()][:2500]:
                _inflight.pop(k, None)
    return f


def ensure(hls, match, fid, i, timeout=90):
    """Path of segment i, fetching it (urgent) and prefetching AHEAD more."""
    table = _tables[fid]
    d, _ = _dirs(hls, match, fid)
    # the segment cache may have been swept (play-cache budget) while the
    # saved layout stayed: recreate it
    d.mkdir(parents=True, exist_ok=True)
    (d / ".touched").write_text(str(int(time.time())))
    out = d / f"s{i:05d}.ts"
    n = len(table["segs"])
    for j in range(i + 1, min(n, i + 1 + AHEAD)):
        if not (d / f"s{j:05d}.ts").exists():
            _submit(hls, match, fid, j, False)
    if out.exists():
        return out
    return _submit(hls, match, fid, i, True).result(timeout=timeout)


def sweep(hls):
    cutoff = time.time() - KEEP_DAYS * 86400
    for d in hls.glob("vn-*"):
        t = d / ".touched"
        try:
            last = float(t.read_text()) if t.exists() else d.stat().st_mtime
        except Exception:  # noqa: BLE001
            last = 0
        if last < cutoff:
            import shutil
            shutil.rmtree(d, ignore_errors=True)
