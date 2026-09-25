#!/usr/bin/env python3
"""dubmux service: on-demand Vietnamese-dub muxing onto high-quality sources.

POST /prepare      {"dub":{"id","url"},"video":{"id","url"}}  -> job state
GET  /status/{dub_id}/{video_id}                               -> job state
GET  /mux/{dub_id}/{video_id}/master.m3u8                      -> HLS (starts the mux)
GET  /mux/{dub_id}/{video_id}/index.m3u8
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
# Matching is CPU/IO heavy (3 concurrent HTTP seeks + decodes + FFTs per
# pair); a prefetch fan-out can queue dozens of pairs at once. Bound it so
# the event loop keeps answering /prepare and /mux promptly.
MATCH_SLOTS = threading.Semaphore(int(os.environ.get("DUBMUX_MATCH_SLOTS", "3")))
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


def _prepare_worker(dub, video, k):
    job = _jobs[k]
    slot_held = False
    try:
        meta = AUDIO / f"{dub['id']}.json"
        # One extraction per dub even when several HQ sources ask at once
        # (PlaybackInfo pairs every HQ release with every dub).
        with _extract_lock(dub["id"]):
            if not meta.exists():
                job["stage"] = "extract"
                args = type("A", (), {"name": dub["id"], "url": dub["url"], "reencode": False,
                                      "parallel": WORKERS})
                dubmux.cmd_extract(args)
        job["stage"] = "match:queued"
        MATCH_SLOTS.acquire()
        slot_held = True
        job["stage"] = "match"
        args = type("A", (), {"name": dub["id"], "video": video["url"], "windows": "90,mid,-150",
                              "span": 120.0, "tolerance": 1.5, "max_lag": 60.0,
                              "min_ratio": 8.0, "max_spread": 0.15})
        # cmd_match prints its report; capture the verdict by re-implementing
        # the small bits we need here so the service owns the JSON.
        # Addon playback URLs are redirectors; ffmpeg needs the final CDN URL
        # to seek. Keep the resolved form for the mux start as well.
        video["url"] = dubmux.resolve_url(video["url"])
        job["video_url"] = video["url"]
        vdur = dubmux.ffprobe_duration(video["url"])
        ddur = json.load(open(meta))["duration"]
        result = {"dub": dub["id"], "video": video["id"], "video_duration": vdur,
                  "dub_duration": ddur, "duration_delta": round(vdur - ddur, 3),
                  "created": int(time.time())}
        if abs(vdur - ddur) > args.tolerance:
            # Different cut (VN encodes drop the ident / credits / preview):
            # try a piecewise alignment and render a track on the video's
            # clock; the mux then uses that track with no offset.
            job["stage"] = "match:piecewise"
            dubfile = str(AUDIO / f"{dub['id']}.m4a")
            rep = align.align(video["url"], dubfile, str(MATCH), k)
            result["piecewise"] = {kk: vv for kk, vv in rep.items() if kk != "aligned"}
            if rep.get("verdict") == "accept":
                result["lag"] = 0.0
                result["verdict"] = "accept"
            else:
                result["verdict"] = "reject:duration"
            hq = MATCH / f"{k}.hq.mka"
            if hq.exists():
                hq.unlink()
        else:
            dubfile = str(AUDIO / f"{dub['id']}.m4a")
            windows = [90.0, vdur / 2, vdur - 150 - args.span]
            windows = [max(0.0, min(w, vdur - args.span - 1)) for w in windows]
            from concurrent.futures import ThreadPoolExecutor

            def one(start):
                a = _decode_hq(video["url"], start, args.span)
                b = dubmux.decode(dubfile, start, args.span)
                lag, peak, ratio = dubmux.xcorr_lag(a, b, args.max_lag)
                method = "waveform"
                if ratio < 6:
                    lag2, peak2, ratio2 = dubmux.xcorr_lag(dubmux.envelope(a), dubmux.envelope(b), args.max_lag)
                    if ratio2 > ratio:
                        lag, peak, ratio, method = lag2, peak2, ratio2, "envelope"
                return {"start": round(start, 1), "lag_s": round(float(lag), 3),
                        "peak_to_rms": round(ratio, 1), "method": method}
            with ThreadPoolExecutor(max_workers=3) as pool:
                wins = list(pool.map(one, windows))
            lags = [w["lag_s"] for w in wins]
            result["windows"] = wins
            result["lag_spread"] = round(max(lags) - min(lags), 3)
            result["lag"] = round(sorted(lags)[1], 3)
            ok = all(w["peak_to_rms"] >= args.min_ratio for w in wins) and result["lag_spread"] <= args.max_spread
            result["verdict"] = "accept" if ok else "reject:correlation"
        json.dump(result, open(_match_path(k), "w"), indent=1)
        job.update(status="ready" if result["verdict"] == "accept" else "rejected",
                   stage="done", result=result)
    except Exception as e:  # noqa: BLE001
        print(f"prepare {k} failed at stage {job.get('stage')}: {str(e)[-600:]}",
              file=sys.stderr, flush=True)
        job.update(status="error", stage="done", error=str(e)[-500:])
    finally:
        if slot_held:
            MATCH_SLOTS.release()
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
    with _lock:
        job = _jobs.get(k)
        if not job or (job["status"] in ("error",) and time.time() - job.get("finished", 0) > 60):
            job = {"status": "running", "stage": "queued", "started": time.time()}
            _jobs[k] = job
            threading.Thread(target=_prepare_worker, args=(dub, video, k), daemon=True).start()
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
        cmd = ["ffmpeg", "-nostdin", "-y", "-v", "warning", *dubmux.ua_for(video_url),
               "-i", video_url, "-itsoffset", f"{-lag:.3f}", "-i", dubfile,
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
            (d / ".done").write_text(str(int(time.time())))
        else:
            (d / ".failed").write_text(str(rc))
        _sweep()
    threading.Thread(target=reaper, daemon=True).start()


@app.get("/mux/{dub_id}/{video_id}/master.m3u8")
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
    if not (_session_dir(k) / ".done").exists():
        m["video_url"] = video_url
        json.dump(m, open(_match_path(k), "w"), indent=1)
        _start_mux(k, dubmux.resolve_url(video_url), dub_id, m["lag"])
    body = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-STREAM-INF:BANDWIDTH=20000000\nindex.m3u8\n"
    return Response(body, media_type="application/vnd.apple.mpegurl",
                    headers={"Cache-Control": "no-store"})


def _wait_for(path: Path, timeout: float):
    end = time.time() + timeout
    while not path.exists():
        if time.time() > end:
            return False
        time.sleep(0.2)
    return True


@app.get("/mux/{dub_id}/{video_id}/index.m3u8")
def index(dub_id: str, video_id: str):
    k = _key(_check_id(dub_id), _check_id(video_id))
    p = _session_dir(k) / "index.m3u8"
    if not _wait_for(p, SEG_WAIT_S):
        raise HTTPException(503, "mux not started")
    text = p.read_text()
    (_session_dir(k) / ".touched").write_text(str(int(time.time())))
    return Response(text, media_type="application/vnd.apple.mpegurl",
                    headers={"Cache-Control": "no-store"})


@app.get("/mux/{dub_id}/{video_id}/{seg}")
def segment(dub_id: str, video_id: str, seg: str):
    k = _key(_check_id(dub_id), _check_id(video_id))
    if not re.match(r"^seg\d{5}\.ts$", seg):
        raise HTTPException(404)
    p = _session_dir(k) / seg
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
            "matches": len(list(MATCH.glob("*.json"))), "hls_sessions": len(hls),
            "running": sum(1 for s in _sessions.values() if s["proc"].poll() is None),
            "hls_bytes": sum(f.stat().st_size for d in hls for f in d.iterdir())}
