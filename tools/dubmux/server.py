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
HLS_BUDGET_GB = float(os.environ.get("DUBMUX_HLS_BUDGET_GB", "300"))
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

def _session_dir(k):
    return HLS / k


def hq_id_of(k):
    return k.split("__", 1)[1]


def _start_mux(k, video_url, dub_id, lag):
    d = _session_dir(k)
    if (d / ".done").exists():
        return
    with _lock:
        if k in _sessions and _sessions[k]["proc"].poll() is None:
            return
        d.mkdir(parents=True, exist_ok=True)
        for f in d.iterdir():
            f.unlink()
        dubfile = str(AUDIO / f"{dub_id}.m4a")
        # video(t) <-> dub(t + lag) (verified against a synthetic delay), so
        # the dub must be shifted by -lag to sit on the video's clock. A
        # piecewise-aligned track already lives on the video's clock.
        aligned = MATCH / f"{k}.aligned.m4a"
        if aligned.exists():
            dubfile, lag = str(aligned), 0.0
        # Pass 2 of the two-pass mux: remux from the raw local copy (no
        # second download); the dub's 0-based clock is shifted by the raw
        # HLS start time. Falls back to the remote release when no copy exists.
        # No start-time shift: ffmpeg normalises both the raw HLS input and
        # the dub to a 0-based clock, and the aligner worked on audio
        # extracted from that same copy (a +1.6 s shift here delayed the dub
        # by exactly that in the first two-pass test).
        raw = _raw_dir(hq_id_of(k))
        if (raw / ".done").exists() and (raw / "index.m3u8").exists():
            (raw / ".touched").write_text(str(int(time.time())))
            src, shift = str(raw / "index.m3u8"), 0.0
        else:
            src, shift = video_url, 0.0
        cmd = ["ffmpeg", "-nostdin", "-y", "-v", "warning", *(() if src != video_url else dubmux.ua_for(video_url)),
               "-i", src, "-itsoffset", f"{shift - lag:.3f}", "-i", dubfile,
               # Subtitles are dropped from the TS on purpose: text tracks
               # can't be muxed into MPEG-TS, and remux keeps serving the
               # source's subtitles (embedded via its own extraction routes,
               # addon/vnphim ones as external tracks) unchanged.
               "-map", "0:v:0", "-map", "1:a:0", "-map", "0:a?", "-sn",
               "-c", "copy",
               "-metadata:s:a:0", "language=vie",
               "-metadata:s:a:0", "title=Tiếng Việt (Thuyết Minh)",
               "-disposition:a:0", "default",
               "-f", "hls", "-hls_time", "6", "-hls_playlist_type", "event",
               "-hls_flags", "temp_file+independent_segments",
               "-hls_segment_filename", str(d / "seg%05d.ts"), str(d / "index.m3u8")]
        log = open(d / "ffmpeg.log", "w")
        proc = subprocess.Popen(cmd, stdout=log, stderr=log)
        _sessions[k] = {"proc": proc, "started": time.time()}

    def reaper():
        rc = proc.wait()
        if rc == 0:
            ok, report = _verify_sync(d)
            (d / ".verify").write_text(json.dumps(report))
            if ok:
                (d / ".done").write_text(str(int(time.time())))
            else:
                print(f"mux {k} FAILED sync self-check: {report}", file=sys.stderr, flush=True)
                (d / ".failed").write_text("sync")
        else:
            (d / ".failed").write_text(str(rc))
        _sweep()
    threading.Thread(target=reaper, daemon=True).start()


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
    cmd = ["ffmpeg", "-v", "error", "-nostdin", "-i", path, "-map", f"0:a:{track}", "-vn", "-sn",
           "-ac", "1", "-ar", str(dubmux.RATE), "-f", "f32le", "-"]
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
    # next-episode pre-mux — may start a full-episode mux.
    if request.method == "HEAD":
        return Response(status_code=200, media_type="application/vnd.apple.mpegurl",
                        headers={"Cache-Control": "no-store"})
    _ensure_mux(k, dub_id, m, video_url)
    # Stream-copy muxes finish in seconds to a minute; VidHub treats an
    # in-progress EVENT playlist as a live stream (length 0, no seeking,
    # stops after the segments it first saw), so wait for the VOD playlist.
    _wait_for(_session_dir(k) / ".done", MASTER_WAIT_S)
    (_session_dir(k) / ".touched").write_text(str(int(time.time())))
    body = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-STREAM-INF:BANDWIDTH=20000000\nindex.m3u8\n"
    return Response(body, media_type="application/vnd.apple.mpegurl",
                    headers={"Cache-Control": "no-store"})


def _ensure_mux(k, dub_id, m, video_url):
    if (_session_dir(k) / ".done").exists():
        return
    m["video_url"] = video_url
    json.dump(m, open(_match_path(k), "w"), indent=1)
    _start_mux(k, dubmux.resolve_url(video_url), dub_id, m["lag"])


@app.post("/mux/{dub_id}/{video_id}/touch")
def touch(dub_id: str, video_id: str):
    """Extend a finished mux's retention as if it had just been played.
    remux calls this for every dub row of the episodes around the one being
    watched (next 10, previous 2): the viewer will most likely get to them, so
    their muxes should not age out first. Never starts a mux."""
    d = _session_dir(_key(_check_id(dub_id), _check_id(video_id)))
    if not (d / ".done").exists():
        return {"touched": False}
    (d / ".touched").write_text(str(int(time.time())))
    return {"touched": True}


@app.post("/mux/{dub_id}/{video_id}/start")
def start(dub_id: str, video_id: str, request: Request):
    """Start (or confirm) the mux for an accepted pair without waiting —
    remux calls this for every row of the episode being played and for
    the next episode's first pair, so a later player GET finds a VOD."""
    k = _key(_check_id(dub_id), _check_id(video_id))
    m = _load_match(k)
    if not m or m.get("verdict") != "accept":
        raise HTTPException(409, "dub not prepared or rejected for this source")
    video_url = request.query_params.get("video") or m.get("video_url")
    if not video_url:
        raise HTTPException(400, "video url required (query ?video=)")
    d = _session_dir(k)
    if not (d / ".done").exists():
        _ensure_mux(k, dub_id, m, video_url)
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
    p = _session_dir(k) / "index.m3u8"
    if request.method == "HEAD":
        return Response(status_code=200 if p.exists() or _load_match(k) else 404,
                        media_type="application/vnd.apple.mpegurl")
    if not _wait_for(p, SEG_WAIT_S):
        raise HTTPException(503, "mux not started")
    d = _session_dir(k)
    if not (d / ".done").exists() and not (d / ".failed").exists():
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
    p = _session_dir(k) / seg
    if request.method == "HEAD":
        return Response(status_code=200 if p.exists() else 404, media_type="video/mp2t")
    if not p.exists():
        d = _session_dir(k)
        if (d / ".done").exists() or (d / ".failed").exists():
            raise HTTPException(404)
        if not _wait_for(p, SEG_WAIT_S):
            raise HTTPException(503, "segment not produced yet")
    return FileResponse(str(p), media_type="video/mp2t",
                        headers={"Cache-Control": "private, max-age=3600"})


# ------------------------------------------------------------ retention ---

def _sweep():
    now = time.time()
    cutoff = now - RETENTION_DAYS * 86400
    for f in list(AUDIO.glob("*.m4a")) + list(MATCH.glob("*.json")):
        if f.stat().st_mtime < cutoff:
            f.unlink(missing_ok=True)
            f.with_suffix(".json").unlink(missing_ok=True)
    dirs = []
    for d in HLS.iterdir():
        if not d.is_dir():
            continue
        if d.name.startswith("raw-"):
            # Raw copies are staging, not cache: gone RAW_KEEP_S after their
            # last use, immediately if the copy failed or is in use by nobody
            # and no accepted match refers to the release.
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
        if k := d.name:
            if k in _sessions and _sessions[k]["proc"].poll() is None:
                continue
        t = d / ".touched"
        last = float(t.read_text()) if t.exists() else d.stat().st_mtime
        size = sum(f.stat().st_size for f in d.iterdir())
        if last < cutoff or (d / ".failed").exists():
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


# Retention on startup too: a prefetch sweep can complete many muxes while the
# reaper-time sweep only ever runs on this process's own completions.
threading.Thread(target=_sweep, daemon=True).start()


@app.get("/health")
def health():
    hls = [d for d in HLS.iterdir() if d.is_dir()]
    return {"ok": True, "audio_tracks": len(list(AUDIO.glob("*.m4a"))),
            "matches": len(list(MATCH.glob("*.json"))), "hls_sessions": len([h for h in hls if not h.name.startswith("raw-")]),
            "raw_copies": len([h for h in hls if h.name.startswith("raw-")]),
            "running": sum(1 for s in _sessions.values() if s["proc"].poll() is None),
            "hls_bytes": sum(f.stat().st_size for d in hls for f in d.iterdir())}
