#!/usr/bin/env python3
"""dubmux service: on-demand Vietnamese-dub muxing onto high-quality sources.

POST /prepare  {dub:{id,url}, video:{id,url}, wait, priority}  (priority: lower = sooner; re-POST to bump)      {"dub":{"id","url"},"video":{"id","url"}}  -> job state
GET  /status/{dub_id}/{video_id}                               -> job state
GET  /mux/{dub_id}/{video_id}/master.m3u8                      -> HLS (starts the mux)
GET  /mux/{dub_id}/{video_id}/index.m3u8                       (VOD once the mux is done; waits up to MASTER_WAIT_S)
POST /mux/{dub_id}/{video_id}/start                             -> start the mux, return at once
POST /mux/{dub_id}/{video_id}/touch                             -> extend a finished mux's retention (never starts one)
GET  /mux/{dub_id}/{video_id}/seg{n}.ts
GET  /health

State on disk under DUBMUX_DATA:
  audio/<dub_id>.m4a + .json      extracted dub tracks
  match/<dub_id>__<video_id>.json cross-correlation result
  hls/<dub_id>__<video_id>/       segments + playlist (+ .done when VOD)
"""
import asyncio, json, os, re, shutil, subprocess, sys, threading, time
from pathlib import Path

from fastapi import FastAPI, HTTPException, Request
from fastapi.responses import FileResponse, JSONResponse, Response

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import dubmux  # noqa: E402
import align  # noqa: E402

DATA = Path(os.environ.get("DUBMUX_DATA", "/data"))
AUDIO, MATCH, HLS = DATA / "audio", DATA / "match", DATA / "hls"
for d in (AUDIO, MATCH, HLS):
    d.mkdir(parents=True, exist_ok=True)
dubmux.CACHE = str(AUDIO)

RETENTION_DAYS = int(os.environ.get("DUBMUX_RETENTION_DAYS", "30"))
HLS_BUDGET_GB = float(os.environ.get("DUBMUX_HLS_BUDGET_GB", "100"))
WORKERS = int(os.environ.get("DUBMUX_FETCH_WORKERS", "128"))
SEG_WAIT_S = 25
# A player GET on the master/index waits this long for the mux to finish so
# it gets a VOD playlist (duration + seeking); an EVENT playlist is served
# only when the mux is still running after that.
MASTER_WAIT_S = float(os.environ.get("DUBMUX_MASTER_WAIT_S", "50"))
ID_RE = re.compile(r"^[A-Za-z0-9._-]{1,120}$")

app = FastAPI(title="dubmux")
_lock = threading.Lock()
_jobs: dict[str, dict] = {}       # prepare jobs keyed dub__video
_sessions: dict[str, dict] = {}   # running ffmpeg muxes keyed dub__video


def _check_id(v):
    if not ID_RE.match(v or ""):
        raise HTTPException(400, "bad id")
    return v


def _key(dub_id, video_id):
    return f"{dub_id}__{video_id}"


# ---------------------------------------------------------------- prepare ---

def _match_path(k):
    return MATCH / f"{k}.json"


def _load_match(k):
    p = _match_path(k)
    if p.exists():
        return json.load(open(p))
    return None


_extract_locks: dict[str, threading.Lock] = {}


class PriorityGate:
    """N concurrent slots handed out lowest-`priority` first (ties: FIFO).
    A waiter's priority can be raised later (`bump`) — remux re-submits a
    pair with a better priority when the viewer gets closer to it."""

    def __init__(self, slots, reserved=0, reserved_max_priority=99):
        # `reserved` extra slots may only be taken by jobs whose priority is
        # <= reserved_max_priority: an interactive request (playback 0, the
        # on-play walk 100+… no) never waits behind a queue of background /
        # evaluation extractions that already hold the shared slots.
        self.slots = slots
        self.reserved = reserved
        self.reserved_max = reserved_max_priority
        self.cv = threading.Condition()
        self.waiting: dict[str, list] = {}   # key -> [priority, seq]
        self.seq = 0
        self.active = 0

    def _capacity(self, priority):
        return self.slots + (self.reserved if priority <= self.reserved_max else 0)

    def acquire(self, key, priority):
        with self.cv:
            self.seq += 1
            self.waiting[key] = [priority, self.seq]
            while True:
                prio = self.waiting[key][0]
                if self.active < self._capacity(prio):
                    # lowest-priority-number waiter that fits in the capacity
                    # its own priority allows
                    eligible = [(v[0], v[1], k) for k, v in self.waiting.items()
                                if self.active < self._capacity(v[0])]
                    best = min(eligible)[2]
                    if best == key:
                        del self.waiting[key]
                        self.active += 1
                        return
                self.cv.wait(1.0)

    def release(self):
        with self.cv:
            self.active -= 1
            self.cv.notify_all()

    def bump(self, key, priority):
        with self.cv:
            w = self.waiting.get(key)
            if w and priority < w[0]:
                w[0] = priority
                self.cv.notify_all()

    def snapshot(self):
        with self.cv:
            return {"active": self.active, "slots": self.slots, "reserved": self.reserved,
                    "waiting": sorted((p, k[:8]) for k, (p, _s) in self.waiting.items())}


# Matching is CPU/IO heavy (3 concurrent HTTP seeks + decodes + FFTs per
# pair); a prefetch fan-out can queue dozens of pairs at once. Bound it so
# the event loop keeps answering /prepare and /mux promptly. Extraction is
# bounded to what the VN extractor runs in parallel, so ITS queue never
# holds work this gate would have ordered differently.
MATCH_GATE = PriorityGate(int(os.environ.get("DUBMUX_MATCH_SLOTS", "3")), reserved=1)
EXTRACT_GATE = PriorityGate(int(os.environ.get("DUBMUX_EXTRACT_SLOTS", "2")), reserved=1)
# Raw local copies of releases (two-pass mux): bounded downloads.
RAW_GATE = PriorityGate(int(os.environ.get("DUBMUX_RAW_SLOTS", "2")), reserved=1)
RAW_KEEP_S = int(os.environ.get("DUBMUX_RAW_KEEP_S", str(2 * 3600)))
_raw_locks: dict[str, threading.Lock] = {}
_raw_refs: dict[str, set] = {}      # hq_id -> pair keys currently using the raw copy
DEFAULT_PRIORITY = 500
# Decoded HQ windows are shared across the dubs paired with the same release
# (up to three dubs per episode) for a few minutes.
_hq_windows: dict[tuple, tuple[float, object]] = {}


def _decode_hq(url, start, span):
    key = (url, round(start, 1), span)
    now = time.time()
    with _lock:
        hit = _hq_windows.get(key)
        if hit and now - hit[0] < 600:
            return hit[1]
    data = dubmux.decode(url, start, span)
    with _lock:
        if len(_hq_windows) > 60:
            _hq_windows.clear()
        _hq_windows[key] = (now, data)
    return data


def _extract_lock(dub_id):
    with _lock:
        return _extract_locks.setdefault(dub_id, threading.Lock())


# Temp segment files from an interrupted extraction (container restart,
# killed job) would otherwise sit in the audio cache forever.
for _stray in AUDIO.glob("dubmux-*.ts"):
    _stray.unlink(missing_ok=True)
for _stray in AUDIO.glob("*.m4a.part"):
    _stray.unlink(missing_ok=True)


def _raw_dir(hq_id):
    return HLS / f"raw-{hq_id}"


def _raw_lock(hq_id):
    with _lock:
        return _raw_locks.setdefault(hq_id, threading.Lock())


def _raw_ref(hq_id, k, add=True):
    with _lock:
        refs = _raw_refs.setdefault(hq_id, set())
        (refs.add if add else refs.discard)(k)
        return len(refs)


def _raw_copy(hq_id, video_url, k, priority):
    """Pass 1 of the two-pass mux: stream-copy the release ONCE into a local
    HLS (video + every original audio track, no dub). Shared by every dub
    paired with this release; the alignment reads its audio from local disk
    and the final mux (pass 2) is a local remux. Returns the dir or None."""
    d = _raw_dir(hq_id)
    with _raw_lock(hq_id):
        if (d / ".done").exists():
            (d / ".touched").write_text(str(int(time.time())))
            return d
        RAW_GATE.acquire("raw:" + k, priority)
        try:
            if (d / ".done").exists():
                return d
            if d.exists():
                shutil.rmtree(d, ignore_errors=True)
            d.mkdir(parents=True, exist_ok=True)
            (d / ".touched").write_text(str(int(time.time())))
            cmd = ["ffmpeg", "-nostdin", "-y", "-v", "error", *dubmux.ua_for(video_url),
                   "-i", video_url, "-map", "0:v:0", "-map", "0:a?", "-sn", "-c", "copy",
                   "-f", "hls", "-hls_time", "6", "-hls_playlist_type", "vod",
                   "-hls_flags", "temp_file+independent_segments",
                   "-hls_segment_filename", str(d / "seg%05d.ts"), str(d / "index.m3u8")]
            with open(d / "ffmpeg.log", "w") as log:
                rc = subprocess.run(cmd, stdout=log, stderr=log).returncode
            if rc != 0 or not (d / "index.m3u8").exists():
                shutil.rmtree(d, ignore_errors=True)
                return None
            # The release's first audio track, on a 0-based clock, for the aligner.
            a = subprocess.run(["ffmpeg", "-nostdin", "-y", "-v", "error", "-i", str(d / "index.m3u8"),
                                "-vn", "-sn", "-map", "0:a:0", "-c:a", "copy", "-f", "matroska",
                                str(d / "audio.mka")], capture_output=True)
            if a.returncode != 0:
                shutil.rmtree(d, ignore_errors=True)
                return None
            (d / ".done").write_text(str(int(time.time())))
            _record_segtab(hq_id, d)
            return d
        finally:
            RAW_GATE.release()


def _raw_start_time(d):
    """PTS the raw HLS starts at (mpegts adds ~1.4 s); the dub input must be
    shifted by it in the final mux because its own clock starts at 0."""
    try:
        out = dubmux.run(["ffprobe", "-v", "error", "-show_entries", "format=start_time",
                          "-of", "csv=p=0", str(d / "index.m3u8")]).stdout.decode().strip()
        return float(out)
    except Exception:  # noqa: BLE001
        return 0.0


def _raw_release(hq_id, k, rejected):
    """Drop the raw copy as soon as it is garbage: a rejected pair releases
    it, and if no other job uses it and no accepted match exists for this
    release (whose mux may still need it), it goes right away. Accepted
    ones are kept RAW_KEEP_S after the last use (_sweep)."""
    left = _raw_ref(hq_id, k, add=False)
    if left or not rejected:
        return
    if any(json.load(open(f)).get("verdict") == "accept"
           for f in MATCH.glob(f"*__{hq_id}.json") if f.is_file()):
        return
    shutil.rmtree(_raw_dir(hq_id), ignore_errors=True)


def _prepare_worker(dub, video, k):
    job = _jobs[k]
    slot_held = False
    hq_id = video["id"]
    raw_thread = None
    raw_box: dict = {}
    rejected = True
    try:
        # Interactive requests (someone is waiting): download the release in
        # parallel with the dub extraction. Background/evaluation ones wait
        # for the pre-screen so a rejected pair never downloads the file.
        video["url"] = dubmux.resolve_url(video["url"])
        job["video_url"] = video["url"]
        _raw_ref(hq_id, k)
        if job["priority"] <= 99 and not (_raw_dir(hq_id) / ".done").exists():
            def _dl():
                raw_box["dir"] = _raw_copy(hq_id, video["url"], k, job["priority"])
            raw_thread = threading.Thread(target=_dl, daemon=True)
            raw_thread.start()
        meta = AUDIO / f"{dub['id']}.json"
        # One extraction per dub even when several HQ sources ask at once
        # (PlaybackInfo pairs every HQ release with every dub).
        with _extract_lock(dub["id"]):
            if not meta.exists():
                job["stage"] = "extract:queued"
                EXTRACT_GATE.acquire(k, job["priority"])
                try:
                    job["stage"] = "extract"
                    args = type("A", (), {"name": dub["id"], "url": dub["url"], "reencode": False,
                                          "parallel": WORKERS})
                    dubmux.cmd_extract(args)
                finally:
                    EXTRACT_GATE.release()
        job["stage"] = "match:queued"
        MATCH_GATE.acquire(k, job["priority"])
        slot_held = True
        job["stage"] = "prescreen"
        vdur = dubmux.ffprobe_duration(video["url"])
        ddur = json.load(open(meta))["duration"]
        dubfile = str(AUDIO / f"{dub['id']}.m4a")
        result = {"dub": dub["id"], "video": video["id"], "video_duration": vdur,
                  "dub_duration": ddur, "duration_delta": round(vdur - ddur, 3),
                  "created": int(time.time())}
        # Pre-screen: 8 windows by byte range (~1 % of the file). Fewer than
        # 3 confident windows = a different edit or the wrong episode; stop
        # before reading the whole release.
        pre = dubmux.prescreen(video["url"], dubfile, vdur, ddur, max_lag=max(75.0, abs(vdur - ddur) + 15.0))
        conf = [(t, lag, r) for t, lag, r in pre if lag is not None and r >= 10.0]
        result["prescreen"] = {"windows": pre, "confident": len(conf)}
        if len(conf) < 3:
            result["verdict"] = "reject:prescreen"
        else:
            lags = sorted(l for _t, l, _r in conf)
            spread = lags[-1] - lags[0]
            same_cut = abs(vdur - ddur) <= 1.5 and len(conf) >= 5 and spread <= 0.15
            if same_cut:
                # Same cut: one constant offset, no piecewise, no full audio needed.
                result["lag"] = round(lags[len(lags) // 2], 3)
                result["lag_spread"] = round(spread, 3)
                result["verdict"] = "accept"
                result["match"] = "prescreen"
            else:
                # A dub already aligned for another release of the same cut
                # (TB vs PM copies of one source, a re-encode) fits this one
                # too: check each known aligned track of this dub with the
                # same cheap windows before reading the release.
                reused = None
                for cand in sorted(MATCH.glob(f"{dub['id']}__*.aligned.m4a")):
                    job["stage"] = "prescreen:aligned"
                    pre2 = dubmux.prescreen(video["url"], str(cand), vdur, vdur, max_lag=5.0)
                    c2 = [l for _t, l, r in pre2 if l is not None and r >= 10.0]
                    if len(c2) >= 5 and max(c2) - min(c2) <= 0.15 and abs(sorted(c2)[len(c2) // 2]) <= 0.2:
                        reused = cand
                        result["prescreen_aligned"] = {"track": cand.name, "confident": len(c2)}
                        break
                if reused is not None:
                    result["lag"] = 0.0
                    result["verdict"] = "accept"
                    result["match"] = "aligned-reuse"
                    result["aligned"] = reused.name
                    _aligned_note(reused, hq_id, video["url"])
                else:
                    job["stage"] = "match:raw"
                    if raw_thread is None:
                        raw_box["dir"] = _raw_copy(hq_id, video["url"], k, job["priority"])
                    else:
                        raw_thread.join()
                        raw_thread = None
                    rd = raw_box.get("dir")
                    job["stage"] = "match:piecewise"
                    rep = align.align(video["url"], dubfile, str(MATCH), k,
                                      hq_audio=str(rd / "audio.mka") if rd else None)
                    result["piecewise"] = {kk: vv for kk, vv in rep.items() if kk != "aligned"}
                    if rep.get("verdict") == "accept":
                        result["lag"] = 0.0
                        result["verdict"] = "accept"
                        result["aligned"] = f"{k}.aligned.m4a"
                        _aligned_note(MATCH / f"{k}.aligned.m4a", hq_id, video["url"])
                    else:
                        result["verdict"] = "reject:duration"
                    hq = MATCH / f"{k}.hq.mka"
                    if hq.exists():
                        hq.unlink()
        rejected = result["verdict"] != "accept"
        json.dump(result, open(_match_path(k), "w"), indent=1)
        job.update(status="ready" if result["verdict"] == "accept" else "rejected",
                   stage="done", result=result)
        # Interactive + accepted: the raw copy is (being) downloaded — finish
        # the final mux right away so the row plays without a cold wait.
        if not rejected and job["priority"] <= 99:
            if raw_thread is not None:
                raw_thread.join()
                raw_thread = None
            _start_mux(k, video["url"], dub["id"], result["lag"])
    except Exception as e:  # noqa: BLE001
        print(f"prepare {k} failed at stage {job.get('stage')}: {str(e)[-600:]}",
              file=sys.stderr, flush=True)
        job.update(status="error", stage="done", error=str(e)[-500:])
    finally:
        if slot_held:
            MATCH_GATE.release()
        if raw_thread is not None:
            raw_thread.join()
        _raw_release(hq_id, k, rejected)
        job["finished"] = time.time()


@app.post("/prepare")
async def prepare(req: Request):
    body = await req.json()
    dub, video = body.get("dub") or {}, body.get("video") or {}
    for v in (dub.get("id"), video.get("id")):
        _check_id(v)
    if not (dub.get("url") and video.get("url")):
        raise HTTPException(400, "dub.url and video.url required")
    k = _key(dub["id"], video["id"])
    cached = _load_match(k)
    if cached:
        return {"status": "ready" if cached["verdict"] == "accept" else "rejected",
                "stage": "done", "result": cached, "cached": True}
    # Lower = sooner. remux sends 0 for the episode being played, ~100+ for
    # the on-play walk (next episodes first; a pair's rank within its
    # episode is added, so every episode's FIRST dub version is served before
    # anyone's second variant), ~300+ for the hourly background queue.
    try:
        priority = int(body.get("priority", DEFAULT_PRIORITY))
    except (TypeError, ValueError):
        priority = DEFAULT_PRIORITY
    with _lock:
        job = _jobs.get(k)
        if not job or (job["status"] in ("error",) and time.time() - job.get("finished", 0) > 60):
            job = {"status": "running", "stage": "queued", "started": time.time(),
                   "priority": priority}
            _jobs[k] = job
            threading.Thread(target=_prepare_worker, args=(dub, video, k), daemon=True).start()
        elif job["status"] == "running" and priority < job.get("priority", DEFAULT_PRIORITY):
            job["priority"] = priority
            EXTRACT_GATE.bump(k, priority)
            MATCH_GATE.bump(k, priority)
    wait = float(body.get("wait", 0) or 0)
    deadline = time.time() + min(wait, 60)
    while job["status"] == "running" and time.time() < deadline:
        await asyncio.sleep(0.25)
    return job


@app.get("/status/{dub_id}/{video_id}")
def status(dub_id: str, video_id: str):
    k = _key(_check_id(dub_id), _check_id(video_id))
    cached = _load_match(k)
    if k in _jobs:
        return _jobs[k]
    if cached:
        return {"status": "ready" if cached["verdict"] == "accept" else "rejected",
                "stage": "done", "result": cached, "cached": True}
    return {"status": "unknown"}


# -------------------------------------------------------------------- mux ---
# Two caches (2026-09-27, user policy):
#  - PLAY cache: finished muxes (HLS sessions), DUBMUX_HLS_BUDGET_GB=100 / 30 d,
#    LRU by last play — "instant" replays.
#  - DURABLE cache: dub audio, match records, aligned tracks and each
#    release's SEGMENT TABLE, DUBMUX_AUDIO_BUDGET_GB=200 / 365 d, LRU by
#    last use — ~50 MB per episode, so ~100 shows of 40 episodes.
# Playback of a pair whose mux is not in the play cache is JUST-IN-TIME:
# the cached segment table becomes a VOD playlist served at once, and a
# stream-copy ffmpeg produces the segments from the live release link a
# few seconds ahead of the player (restarted at the seek point on a far
# seek). Segmentation is deterministic for a given release, so the table
# recorded from the raw copy is valid for every later production.
AUDIO_BUDGET_GB = float(os.environ.get("DUBMUX_AUDIO_BUDGET_GB", "200"))
AUDIO_RETENTION_DAYS = int(os.environ.get("DUBMUX_AUDIO_RETENTION_DAYS", "365"))
JIT_AHEAD = 8          # a request this many segments past the producer just waits
_producers: dict[str, dict] = {}   # session key -> {"proc", "start", "cursor", ...}


def _session_dir(k):
    return HLS / k


def hq_id_of(k):
    return k.split("__", 1)[1]


def _segtab_path(hq_id):
    return MATCH / f"segtab-{hq_id}.json"


def _ts_first_pts(path):
    """PTS (seconds) of the first video PES packet of a TS file, or None.
    Reads 64 KB first (PAT/PMT then the keyframe PES: a few packets in);
    only falls back to a bigger read when needed (2 MB x 400 files took
    55 s on the array)."""
    for size in (64 * 1024, 4 * 1024 * 1024):
        with open(path, "rb") as f:
            buf = f.read(size)
        pts = _scan_first_video_pts(buf)
        if pts is not None or len(buf) < size:
            return pts
    return None


def _scan_first_video_pts(buf):
    for off in range(0, len(buf) - 188, 188):
        if buf[off] != 0x47 or not (buf[off + 1] & 0x40):
            continue
        i = off + 4
        if buf[off + 3] & 0x20:              # adaptation field
            i += 1 + buf[off + 4]
        if buf[i:i + 3] != b"\x00\x00\x01" or not (0xE0 <= buf[i + 3] <= 0xEF):
            continue
        if not (buf[i + 7] & 0x80):
            continue
        p = buf[i + 9:i + 14]
        pts = ((p[0] >> 1) & 7) << 30 | p[1] << 22 | (p[2] >> 1) << 15 | p[3] << 7 | p[4] >> 1
        return pts / 90000.0
    return None


def _record_segtab(hq_id, raw_dir):
    """Persist the release's segment table (true start PTS of every raw
    segment + the last one's duration) in the durable cache; it outlives the
    raw copy and the play-cache session. Cumulative EXTINF sums are NOT the
    timestamps (they drifted 1.6 s over one file), and `-segment_times`
    cuts on real PTS."""
    src = raw_dir / "index.m3u8"
    if not src.exists():
        return
    names, durs = [], []
    for line in src.read_text().splitlines():
        if line.startswith("#EXTINF:"):
            durs.append(float(line[8:].split(",")[0]))
        elif line.startswith("seg"):
            names.append(line.strip())
    starts = []
    for n in names:
        t = _ts_first_pts(raw_dir / n)
        if t is None:
            return
        starts.append(round(t, 6))
    if len(starts) != len(durs) or any(b <= a for a, b in zip(starts, starts[1:])):
        return
    # the clock the alignment lag lives on starts at the release's
    # start_time (what `-start_at_zero` subtracted when the lag was measured)
    origin = _start_time(str(src))
    if origin is None:
        return
    json.dump({"v": 3, "starts": starts, "last": durs[-1], "origin": origin},
              open(_segtab_path(hq_id), "w"))


_start_times: dict[str, float] = {}


def _start_time(path):
    """Container start_time (s) as ffmpeg sees it, cached per path."""
    if path in _start_times:
        return _start_times[path]
    try:
        out = subprocess.run(["ffprobe", "-v", "error", "-show_entries", "format=start_time",
                              "-of", "csv=p=0", path], capture_output=True, text=True, timeout=120).stdout
        v = float(out.strip().strip(",") or "nan")
        if v != v:
            v = 0.0
    except Exception:  # noqa: BLE001
        return None
    _start_times[path] = v
    return v


def _segtab(hq_id):
    """(starts, durs, origin) of the release's segments, or None when unknown."""
    p = _segtab_path(hq_id)
    if not p.exists():
        raw = _raw_dir(hq_id)
        if (raw / ".done").exists():
            _record_segtab(hq_id, raw)
        if not p.exists():
            return None
    try:
        t = json.load(open(p))
        if t.get("v", 0) < 3:
            raw = _raw_dir(hq_id)
            if not (raw / ".done").exists():
                return None
            _record_segtab(hq_id, raw)
            t = json.load(open(p))
            if t.get("v", 0) < 3:
                return None
        starts = t["starts"]
        durs = [b - a for a, b in zip(starts, starts[1:])] + [t["last"]]
    except Exception:  # noqa: BLE001
        return None
    os.utime(p, None)
    return (starts, durs, t["origin"]) if starts else None


def _vod_playlist(durs):
    import math
    out = ["#EXTM3U", "#EXT-X-VERSION:3", f"#EXT-X-TARGETDURATION:{math.ceil(max(durs))}",
           "#EXT-X-MEDIA-SEQUENCE:0", "#EXT-X-PLAYLIST-TYPE:VOD", "#EXT-X-INDEPENDENT-SEGMENTS"]
    for i, d in enumerate(durs):
        out.append(f"#EXTINF:{d:.6f},")
        out.append(f"seg{i:05d}.ts")
    out.append("#EXT-X-ENDLIST")
    return "\n".join(out) + "\n"


def _touch_pair(k, dub_id):
    """LRU bookkeeping for the durable cache: bump everything the pair uses."""
    now = None
    m = _load_match(k) or {}
    extra = [MATCH / m["aligned"], MATCH / (m["aligned"][:-4] + ".json")] if m.get("aligned") else []
    for f in (AUDIO / f"{dub_id}.m4a", AUDIO / f"{dub_id}.json", _match_path(k),
              MATCH / f"{k}.aligned.m4a", _segtab_path(hq_id_of(k)), *extra):
        if f.exists():
            os.utime(f, now)


def _aligned_note(path, hq_id, video_url):
    """Sidecar of an aligned track: which releases it is known to fit (the
    release ids and file names), so one track serves them all."""
    note = Path(str(path)[:-len(".m4a")] + ".json")
    try:
        data = json.load(open(note)) if note.exists() else {"releases": []}
    except Exception:  # noqa: BLE001
        data = {"releases": []}
    name = video_url.split("?")[0].rsplit("/", 1)[-1][:120]
    if not any(r.get("hq") == hq_id for r in data["releases"]):
        data["releases"].append({"hq": hq_id, "name": name, "at": int(time.time())})
    json.dump(data, open(note, "w"), indent=1)


def _dub_input(k, dub_id, lag):
    """(file, lag) the mux uses: a piecewise-aligned track sits on the
    video's clock already (lag 0); the match record names which one."""
    m = _load_match(k) or {}
    if m.get("aligned"):
        aligned = MATCH / m["aligned"]
        if aligned.exists():
            return str(aligned), 0.0
    aligned = MATCH / f"{k}.aligned.m4a"
    if aligned.exists():
        return str(aligned), 0.0
    return str(AUDIO / f"{dub_id}.m4a"), lag


def _mux_source(k, video_url):
    raw = _raw_dir(hq_id_of(k))
    if (raw / ".done").exists() and (raw / "index.m3u8").exists():
        (raw / ".touched").write_text(str(int(time.time())))
        return str(raw / "index.m3u8"), False
    return video_url, True


def _jit_cmd(k, src, remote, dubfile, lag, tab, start_seg, d):
    """ffmpeg command producing segments start_seg.. on the release's own
    PTS clock: `-copyts` keeps an input-seeked (`-ss T`) file's ORIGINAL
    timestamps, `-f segment -segment_times` (boundaries relative to the run's
    first packet, indexed per run) cuts at the raw copy's own segment starts,
    which are keyframes.
    So a restarted producer's segments line up exactly with the ones already
    served. (`-start_at_zero` is NOT used: after a seek it shifted the
    clock by 1.6 s; the hls muxer's own `hls_time` splitting was not
    reproducible across a restart either.)
    video(t) <-> dub(t + lag): dub read from S = max(0, T + lag)."""
    starts, durs, origin = tab
    T = starts[start_seg]
    # segment.c: end_pts = times[segment_count(0-based per run)] +
    # reference_stream_first_pts, i.e. boundaries RELATIVE to the run's first
    # packet (= starts[start_seg], the seek lands on the segment start)
    times = ",".join(f"{b - T:.6f}" for b in starts[start_seg + 1:]) or f"{durs[-1] + 1:.6f}"
    # The lag was measured with both inputs re-based to 0 (`-start_at_zero`):
    # decoded video clock v = pts - origin, dub clock u = dub_pts - dub_st,
    # and u = v + lag. On the release's PTS clock the dub therefore needs
    # dub_pts + (origin - dub_st - lag), and video pts T is dub time
    # T - origin + dub_st + lag.
    dub_st = _start_time(dubfile) or 0.0
    shift = origin - dub_st - lag
    # the dub is always input-seeked so no packet lands before 0 (ffmpeg's
    # avoid_negative_ts would otherwise shift the whole run's clock)
    S = max(0.0, T - shift)
    seek_v = ["-ss", f"{T:.6f}"] if start_seg > 0 else []
    seek_d = ["-ss", f"{S:.6f}"] if S > 0 else []
    return ["ffmpeg", "-nostdin", "-y", "-v", "warning", *(dubmux.ua_for(src) if remote else ()),
            "-copyts", *seek_v, "-i", src,
            "-copyts", *seek_d, "-itsoffset", f"{shift:.6f}", "-i", dubfile,
            "-map", "0:v:0", "-map", "1:a:0", "-map", "0:a?", "-sn", "-c", "copy",
            "-metadata:s:a:0", "language=vie", "-metadata:s:a:0", "title=Tiếng Việt (Thuyết Minh)",
            "-disposition:a:0", "default", "-muxdelay", "0", "-muxpreload", "0",
            "-f", "segment", "-segment_format", "mpegts", "-segment_times", times,
            "-segment_time_delta", "0.001", "-segment_start_number", str(start_seg),
            "-reset_timestamps", "0",
            "-segment_list", str(d / "live.m3u8"), "-segment_list_type", "m3u8",
            "-segment_list_flags", "live", str(d / "seg%05d.ts")]


def _run_dir(d, start_seg):
    return d / f"run{start_seg:05d}"


def _seg_ready(d, seg, k):
    """Segments are moved into the session dir only once complete."""
    return (d / seg).exists()


def _listed(rd):
    """Segment files a producer has completed (listed in its live.m3u8
    after the file is closed)."""
    try:
        return [l.strip() for l in (rd / "live.m3u8").read_text().splitlines()
                if l.strip().startswith("seg")]
    except FileNotFoundError:
        return []


def _jit_start(k, video_url, dub_id, lag, tab, start_seg=0, end_seg=None):
    """(Re)start the segment producer of a JIT session at `start_seg`
    (a fill run: up to `end_seg`, exclusive). The producer writes into its
    own run dir; a mover thread moves each segment into the session dir
    the moment it is complete, so a segment file in the session dir is
    always whole, killing a producer never truncates a served file, and a
    run never clobbers segments another run already made."""
    starts, durs, _origin = tab
    d = _session_dir(k)
    d.mkdir(parents=True, exist_ok=True)
    if not (d / "index.m3u8").exists() or not (d / ".jit").exists():
        (d / "index.m3u8").write_text(_vod_playlist(durs))
        (d / ".jit").write_text(json.dumps({"v": 2, "segments": len(durs), "hq": hq_id_of(k)}))
    (d / ".touched").write_text(str(int(time.time())))
    with _lock:
        old = _producers.get(k)
        if old and old["proc"].poll() is None:
            if old["start"] <= start_seg <= old["cursor"] + JIT_AHEAD:
                return  # already producing towards this segment
            old["proc"].kill()
            old["proc"].wait()
        rd = _run_dir(d, start_seg)
        shutil.rmtree(rd, ignore_errors=True)
        rd.mkdir()
        dubfile, lag = _dub_input(k, dub_id, lag)
        src, remote = _mux_source(k, video_url)
        cmd = _jit_cmd(k, src, remote, dubfile, lag, tab, start_seg, rd)
        log = open(d / "ffmpeg.log", "a")
        log.write(f"\n# start_seg={start_seg} end_seg={end_seg} src={'remote' if remote else 'raw'}\n"); log.flush()
        proc = subprocess.Popen(cmd, stdout=log, stderr=log)
        prod = {"proc": proc, "start": start_seg, "cursor": start_seg - 1, "n": len(durs),
                "started": time.time(), "fills": (old or {}).get("fills", 0), "stopped": False}
        _producers[k] = prod
        _touch_pair(k, dub_id)

    def move_ready(final):
        names = _listed(rd)
        if final and proc.returncode == 0:
            # the last file of a finished run is closed but not necessarily listed
            names = sorted(f.name for f in rd.glob("seg*.ts"))
        for name in names:
            f = rd / name
            if f.exists():
                os.replace(f, d / name)
                prod["cursor"] = max(prod["cursor"], int(name[3:8]))

    def runner():
        while proc.poll() is None:
            move_ready(False)
            if end_seg is not None and prod["cursor"] >= end_seg - 1:
                prod["stopped"] = True   # the fill run has reached the existing segments
                proc.kill()
                break
            time.sleep(0.3)
        proc.wait()
        move_ready(True)
        shutil.rmtree(rd, ignore_errors=True)
        with _lock:
            if _producers.get(k) is not prod:
                return  # superseded by a restart
        ok = proc.returncode == 0 or prod["stopped"]
        hole = next((i for i in range(len(durs)) if not (d / f"seg{i:05d}.ts").exists()), None)
        if hole is not None:
            # the viewer seeked around: fill the holes so the session becomes
            # a complete cache entry (each pass covers one hole)
            if ok and prod["fills"] < 200 and not (d / ".failed").exists():
                end = next((i for i in range(hole + 1, len(durs)) if (d / f"seg{i:05d}.ts").exists()), None)
                _jit_start(k, video_url, dub_id, lag, tab, hole, end)
                _producers[k]["fills"] = prod["fills"] + 1
            return
        (d / ".done").write_text(str(int(time.time())))
        ok, report = _verify_sync(d)
        (d / ".verify").write_text(json.dumps(report))
        if not ok:
            print(f"mux {k} FAILED sync self-check: {report}", file=sys.stderr, flush=True)
            (d / ".done").unlink(missing_ok=True)
            (d / ".failed").write_text("sync")
        _sweep()
    threading.Thread(target=runner, daemon=True).start()


def _start_mux(k, video_url, dub_id, lag):
    """Whole-session production (pre-mux / no-table fallback). With a known
    segment table this is a JIT session started at 0; otherwise the raw copy
    is made first (which yields the table) and then the same."""
    d = _session_dir(k)
    if (d / ".done").exists():
        return
    hq = hq_id_of(k)
    tab = _segtab(hq)
    if tab is None:
        raw = _raw_copy(hq, video_url, k, 0)
        if raw:
            _record_segtab(hq, raw)
            tab = _segtab(hq)
    if tab is None:
        raise HTTPException(502, "release could not be read")
    _jit_start(k, video_url, dub_id, lag, tab, 0)


def _verify_sync(d, max_abs_lag=0.35):
    """Self-check of a finished mux: the dub track must line up with the
    release's own first audio track (they carry the same music/effects bed)
    at three points. Catches clock mistakes like the +1.6 s double shift the
    first two-pass build had. Windows with no correlation (ratio < 6) are
    ignored; the check fails only on a CONFIDENT disagreement."""
    try:
        idx = str(d / "index.m3u8")
        # Decode both tracks whole and slice in memory: `-ss` on an HLS input
        # is not sample-accurate in the container's ffmpeg (per-track
        # segment-boundary offsets of up to 1.6 s made a correct mux fail).
        A = _decode_full_track(idx, 0)
        B = _decode_full_track(idx, 1)
        R = dubmux.RATE
        dur = min(len(A), len(B)) / R
        pts = [120.0, dur / 2, max(120.0, dur - 200)]
        lags = []
        for t in pts:
            i0, i1 = int(t * R), int((t + 60) * R)
            a, b = A[i0:i1], B[i0:i1]
            m = min(len(a), len(b))
            if m < 30 * R:
                continue
            lag, _peak, ratio = dubmux.xcorr_lag(a[:m], b[:m], 10.0)
            lags.append((round(t), round(float(lag), 3), round(float(ratio), 1)))
        conf = [l for l in lags if l[2] >= 6.0]
        bad = [l for l in conf if abs(l[1]) > max_abs_lag]
        return (not bad, {"windows": lags, "bad": bad})
    except Exception as e:  # noqa: BLE001
        return (True, {"error": str(e)[-200:]})


def _decode_full_track(path, track):
    # first_pts=0 pads each track with silence from t=0 to its first sample,
    # so both decoded arrays sit on the file's absolute clock (the tracks
    # start at different offsets: the JIT clock is the release's own PTS).
    cmd = ["ffmpeg", "-v", "error", "-nostdin", "-i", path, "-map", f"0:a:{track}", "-vn", "-sn",
           "-af", "aresample=async=1:first_pts=0", "-ac", "1", "-ar", str(dubmux.RATE), "-f", "f32le", "-"]
    import numpy as np
    return np.frombuffer(dubmux.run(cmd).stdout, dtype=np.float32)


def _decode_track(path, track, start, span):
    cmd = ["ffmpeg", "-v", "error", "-nostdin", "-ss", f"{start:.3f}", "-t", f"{span:.3f}",
           "-i", path, "-map", f"0:a:{track}", "-vn", "-sn", "-ac", "1", "-ar", str(dubmux.RATE),
           "-f", "f32le", "-"]
    import numpy as np
    return np.frombuffer(dubmux.run(cmd).stdout, dtype=np.float32)


@app.api_route("/mux/{dub_id}/{video_id}/master.m3u8", methods=["GET", "HEAD"])
def master(dub_id: str, video_id: str, request: Request):
    k = _key(_check_id(dub_id), _check_id(video_id))
    m = _load_match(k)
    if not m or m.get("verdict") != "accept":
        raise HTTPException(409, "dub not prepared or rejected for this source")
    video_url = request.query_params.get("video") or m.get("video_url")
    if not video_url:
        raise HTTPException(400, "video url required (query ?video=)")
    # remux liveness checks (and item-doc probes) HEAD the master while a
    # viewer merely browses; only a GET — a player, or the deliberate
    # pre-mux — may start producing.
    if request.method == "HEAD":
        return Response(status_code=200, media_type="application/vnd.apple.mpegurl",
                        headers={"Cache-Control": "no-store"})
    d = _session_dir(k)
    if (d / ".done").exists() and not _session_consistent(d):
        # a finished session whose playlist does not describe its segments
        # (a legacy producer killed mid-playlist): rebuild rather than serve
        print(f"mux {k}: inconsistent finished session, rebuilding", file=sys.stderr, flush=True)
        shutil.rmtree(d, ignore_errors=True)
    if not (d / ".done").exists():
        if (d / ".jit").exists():
            _resume_session(k)                       # orphaned by a restart? continue it
        elif _segtab(hq_id_of(k)) is not None:
            _ensure_mux(k, dub_id, m, video_url)     # opens the session in ~1 s
        elif not (d / ".jit").exists():
            # no segment table yet (a match prepared before tables existed):
            # the raw copy has to be made first — start it (deduplicated per
            # release by _raw_copy's lock) and wait for the session to open
            threading.Thread(target=_ensure_mux, args=(k, dub_id, m, video_url), daemon=True).start()
            _wait_for(d / ".jit", MASTER_WAIT_S)
    (d / ".touched").write_text(str(int(time.time())))
    body = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-STREAM-INF:BANDWIDTH=20000000\nindex.m3u8\n"
    return Response(body, media_type="application/vnd.apple.mpegurl",
                    headers={"Cache-Control": "no-store"})


def _session_consistent(d):
    """A finished session's playlist must end (VOD) and list every segment."""
    try:
        idx = (d / "index.m3u8").read_text()
    except OSError:
        return False
    n_files = sum(1 for _ in d.glob("seg*.ts"))
    return "#EXT-X-ENDLIST" in idx and idx.count("#EXTINF") == n_files > 0


def _ensure_mux(k, dub_id, m, video_url):
    if (_session_dir(k) / ".done").exists():
        return
    m["video_url"] = video_url
    json.dump(m, open(_match_path(k), "w"), indent=1)
    _start_mux(k, dubmux.resolve_url(video_url), dub_id, m["lag"])


@app.post("/mux/{dub_id}/{video_id}/touch")
def touch(dub_id: str, video_id: str):
    """Extend a finished mux's retention as if it had just been played, and
    the pair's durable-cache entries as if just used. Never starts anything."""
    k = _key(_check_id(dub_id), _check_id(video_id))
    _touch_pair(k, dub_id)
    d = _session_dir(k)
    if not (d / ".done").exists():
        return {"touched": False}
    (d / ".touched").write_text(str(int(time.time())))
    return {"touched": True}


@app.post("/mux/{dub_id}/{video_id}/start")
def start(dub_id: str, video_id: str, request: Request):
    """Start (or confirm) full production for an accepted pair without
    waiting — the pre-mux for the next episode / the episode being opened."""
    k = _key(_check_id(dub_id), _check_id(video_id))
    m = _load_match(k)
    if not m or m.get("verdict") != "accept":
        raise HTTPException(409, "dub not prepared or rejected for this source")
    video_url = request.query_params.get("video") or m.get("video_url")
    if not video_url:
        raise HTTPException(400, "video url required (query ?video=)")
    d = _session_dir(k)
    if not (d / ".done").exists():
        threading.Thread(target=_ensure_mux, args=(k, dub_id, m, video_url), daemon=True).start()
    return {"ok": True, "done": (d / ".done").exists()}


def _wait_for(path: Path, timeout: float):
    end = time.time() + timeout
    while not path.exists():
        if time.time() > end:
            return False
        time.sleep(0.2)
    return True


@app.api_route("/mux/{dub_id}/{video_id}/index.m3u8", methods=["GET", "HEAD"])
def index(dub_id: str, video_id: str, request: Request):
    k = _key(_check_id(dub_id), _check_id(video_id))
    d = _session_dir(k)
    p = d / "index.m3u8"
    if request.method == "HEAD":
        return Response(status_code=200 if p.exists() or _load_match(k) else 404,
                        media_type="application/vnd.apple.mpegurl")
    if not _wait_for(p, SEG_WAIT_S):
        raise HTTPException(503, "mux not started")
    if not (d / ".jit").exists() and not (d / ".done").exists() and not (d / ".failed").exists():
        _wait_for(d / ".done", MASTER_WAIT_S)
    text = p.read_text()
    if (d / ".done").exists():
        text = text.replace("#EXT-X-PLAYLIST-TYPE:EVENT", "#EXT-X-PLAYLIST-TYPE:VOD", 1)
    (d / ".touched").write_text(str(int(time.time())))
    return Response(text, media_type="application/vnd.apple.mpegurl",
                    headers={"Cache-Control": "no-store"})


@app.api_route("/mux/{dub_id}/{video_id}/{seg}", methods=["GET", "HEAD"])
def segment(dub_id: str, video_id: str, seg: str, request: Request):
    k = _key(_check_id(dub_id), _check_id(video_id))
    if not re.match(r"^seg\d{5}\.ts$", seg):
        raise HTTPException(404)
    d = _session_dir(k)
    p = d / seg
    if request.method == "HEAD":
        return Response(status_code=200 if p.exists() else 404, media_type="video/mp2t")
    if not _seg_ready(d, seg, k):
        if (d / ".jit").exists() and not (d / ".done").exists():
            n = int(seg[3:8])
            meta = json.loads((d / ".jit").read_text())
            m = _load_match(k) or {}
            tab = _segtab(meta["hq"])
            prod = _producers.get(k)
            running = bool(prod and prod["proc"].poll() is None)
            if tab and (not running or n < prod["start"] or n > prod["cursor"] + JIT_AHEAD):
                # far seek (or producer gone): restart at this segment
                _jit_start(k, m.get("video_url", ""), dub_id, m.get("lag", 0.0), tab, n)
        elif (d / ".done").exists() or (d / ".failed").exists():
            raise HTTPException(404)
        end = time.time() + SEG_WAIT_S
        while not _seg_ready(d, seg, k):
            if time.time() > end:
                raise HTTPException(503, "segment not produced yet")
            time.sleep(0.2)
    (d / ".touched").write_text(str(int(time.time())))
    return FileResponse(str(p), media_type="video/mp2t",
                        headers={"Cache-Control": "private, max-age=3600"})


# ------------------------------------------------------------ retention ---

def _sweep():
    now = time.time()
    # Durable cache (audio, matches, aligned tracks, segment tables): LRU by
    # mtime (bumped on every use), 365 d / 200 GB.
    cutoff = now - AUDIO_RETENTION_DAYS * 86400
    files = [f for f in list(AUDIO.iterdir()) + list(MATCH.iterdir()) if f.is_file()]
    for f in files:
        if f.stat().st_mtime < cutoff:
            f.unlink(missing_ok=True)
    files = [f for f in files if f.exists()]
    total = sum(f.stat().st_size for f in files)
    for f in sorted(files, key=lambda f: f.stat().st_mtime):
        if total <= AUDIO_BUDGET_GB * 1e9:
            break
        total -= f.stat().st_size
        f.unlink(missing_ok=True)
    # Play cache + staging.
    cutoff = now - RETENTION_DAYS * 86400
    dirs = []
    for d in HLS.iterdir():
        if not d.is_dir():
            continue
        if d.name.startswith("raw-"):
            hq = d.name[4:]
            t = d / ".touched"
            last = float(t.read_text()) if t.exists() else d.stat().st_mtime
            in_use = bool(_raw_refs.get(hq))
            if not in_use and (not (d / ".done").exists() and now - last > 1800
                               or now - last > RAW_KEEP_S):
                shutil.rmtree(d, ignore_errors=True)
                continue
            dirs.append((last, sum(f.stat().st_size for f in d.iterdir() if f.is_file()), d))
            continue
        k = d.name
        prod = _producers.get(k)
        if prod and prod["proc"].poll() is None:
            continue
        if k in _sessions and _sessions[k]["proc"].poll() is None:
            continue
        t = d / ".touched"
        last = float(t.read_text()) if t.exists() else d.stat().st_mtime
        size = sum(f.stat().st_size for f in d.iterdir() if f.is_file())
        # Failed sessions and half-produced JIT sessions nobody has touched for
        # an hour are garbage, not cache.
        if last < cutoff or (d / ".failed").exists() or \
           ((d / ".jit").exists() and not (d / ".done").exists() and now - last > 3600):
            shutil.rmtree(d, ignore_errors=True)
            continue
        dirs.append((last, size, d))
    total = sum(s for _, s, _ in dirs)
    for last, size, d in sorted(dirs):
        if total <= HLS_BUDGET_GB * 1e9:
            break
        shutil.rmtree(d, ignore_errors=True)
        total -= size


@app.get("/jobs")
def jobs():
    """In-memory preparation jobs (this process lifetime) for debugging."""
    return {k: {kk: vv for kk, vv in v.items() if kk != "result"} for k, v in _jobs.items()}


@app.get("/queue")
def queue():
    """Gate occupancy + waiting pairs by priority (lowest first)."""
    return {"extract": EXTRACT_GATE.snapshot(), "match": MATCH_GATE.snapshot()}


@app.post("/sweep")
def sweep_now():
    """Apply retention / budget immediately (also runs at startup)."""
    _sweep()
    return health()


def _resume_session(k):
    """Continue an unfinished JIT session (after a restart, or when a
    finished-with-holes session is opened again): drop stale run dirs and
    fill from the first hole. Returns True when a producer was started."""
    d = _session_dir(k)
    if not (d / ".jit").exists() or (d / ".done").exists() or (d / ".failed").exists():
        return False
    prod = _producers.get(k)
    if prod and prod["proc"].poll() is None:
        return False
    for rd in d.glob("run*"):
        shutil.rmtree(rd, ignore_errors=True)
    m = _load_match(k) or {}
    tab = _segtab(hq_id_of(k))
    if not m.get("video_url") or tab is None:
        return False
    n = len(tab[0])
    hole = next((i for i in range(n) if not (d / f"seg{i:05d}.ts").exists()), None)
    if hole is None:
        (d / ".done").write_text(str(int(time.time())))
        return False
    end = next((i for i in range(hole + 1, n) if (d / f"seg{i:05d}.ts").exists()), None)
    _jit_start(k, dubmux.resolve_url(m["video_url"]), k.split("__", 1)[0], m.get("lag", 0.0), tab, hole, end)
    return True


def _startup():
    _sweep()
    # producers do not survive a restart: resume every unfinished session
    for d in sorted(HLS.iterdir(), key=lambda p: p.stat().st_mtime, reverse=True):
        if d.is_dir() and not d.name.startswith("raw-"):
            try:
                if _resume_session(d.name):
                    print(f"resumed JIT session {d.name}", file=sys.stderr, flush=True)
            except Exception as e:  # noqa: BLE001
                print(f"resume {d.name} failed: {e}", file=sys.stderr, flush=True)


# Retention on startup too: a prefetch sweep can complete many muxes while the
# reaper-time sweep only ever runs on this process's own completions.
threading.Thread(target=_startup, daemon=True).start()


def _dir_bytes(d):
    try:
        return sum(f.stat().st_size for f in d.iterdir() if f.is_file())
    except FileNotFoundError:
        return 0


@app.get("/health")
def health():
    hls = [d for d in HLS.iterdir() if d.is_dir()]
    return {"ok": True, "audio_tracks": len(list(AUDIO.glob("*.m4a"))),
            "matches": len(list(MATCH.glob("*.json"))), "hls_sessions": len([h for h in hls if not h.name.startswith("raw-")]),
            "raw_copies": len([h for h in hls if h.name.startswith("raw-")]),
            "running": sum(1 for s in _sessions.values() if s["proc"].poll() is None),
            "hls_bytes": sum(_dir_bytes(d) for d in hls),
            "durable_bytes": sum(f.stat().st_size for f in list(AUDIO.iterdir()) + list(MATCH.iterdir()) if f.is_file()),
            "segtabs": len(list(MATCH.glob("segtab-*.json")))}
