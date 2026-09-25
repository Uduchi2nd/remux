#!/usr/bin/env python3
"""vnext — Vietnamese-side dub audio extractor (runs on the VN Proxmox, LXC 100).

kkphim/ophim segments are geo-blocked outside VN and, from abroad, reach the
seedbox only through the VN MediaFlow over the home uplink (~2.4 MB/s). This
service takes vnphim's stripped playlist, unwraps every MediaFlow-wrapped
segment back to its kkphim/ophim origin URL (+ the Referer MediaFlow would
have injected), fetches them with domestic bandwidth (64 connections
≈ 7 MB/s measured), demuxes the AAC with ffmpeg on the fly, and serves the
~70 MB result to the seedbox over the tailnet.

POST /extract {"id": "<dub id>", "url": "<vnphim playlist url>"} -> job
GET  /extract/{id}            -> {"status": queued|running|ready|error, ...}
GET  /extract/{id}/audio      -> the .m4a
GET  /health
"""
import json, os, shutil, subprocess, sys, threading, time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from urllib.parse import parse_qs, urljoin, urlsplit

from fastapi import FastAPI, HTTPException, Request
from fastapi.responses import FileResponse

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import dubmux  # noqa: E402  (http_get, parse_media_playlist, ffprobe_duration)

DATA = Path(os.environ.get("VNEXT_DATA", "/data"))
DATA.mkdir(parents=True, exist_ok=True)
WORKERS = int(os.environ.get("VNEXT_WORKERS", "64"))
WINDOW = int(os.environ.get("VNEXT_WINDOW", "48"))     # segments in flight (memory bound)
TTL_HOURS = float(os.environ.get("VNEXT_TTL_HOURS", "12"))
UA = "Mozilla/5.0"

app = FastAPI(title="vnext")
_lock = threading.Lock()
_jobs: dict[str, dict] = {}
_slots = threading.Semaphore(int(os.environ.get("VNEXT_JOBS", "2")))


def unwrap(seg_url):
    """MediaFlow `/proxy/stream?d=<origin>&h_Referer=<ref>` -> (origin, headers)."""
    parts = urlsplit(seg_url)
    if "/proxy/stream" in parts.path or "/proxy/hls" in parts.path:
        q = parse_qs(parts.query)
        origin = q.get("d", [None])[0]
        if origin:
            hdr = {"User-Agent": UA}
            for k, v in q.items():
                if k.startswith("h_") and v:
                    hdr[k[2:]] = v[0]
            return origin, hdr
    return seg_url, {"User-Agent": UA}


def fetch(seg):
    url, byterange = seg
    origin, hdr = unwrap(url)
    if byterange:
        hdr["Range"] = f"bytes={byterange[0]}-{byterange[0] + byterange[1] - 1}"
    last = None
    for attempt in range(4):
        try:
            import urllib.request
            with urllib.request.urlopen(urllib.request.Request(origin, headers=hdr), timeout=60) as r:
                data = r.read()
            if byterange and len(data) != byterange[1]:
                raise IOError("short range read")
            return data
        except Exception as e:  # noqa: BLE001
            last = e
            time.sleep(0.5 * (attempt + 1))
    raise last


def _run(job_id, url):
    job = _jobs[job_id]
    out = DATA / f"{job_id}.m4a"
    with _slots:
        job.update(status="running", started=time.time())
        try:
            _, segs = dubmux.parse_media_playlist(url)
            job["segments"] = len(segs)
            ff = subprocess.Popen(
                ["ffmpeg", "-nostdin", "-y", "-v", "error", "-i", "pipe:0", "-vn", "-sn",
                 "-map", "0:a:0", "-c:a", "copy", "-movflags", "+faststart", "-f", "ipod",
                 str(out) + ".part"],
                stdin=subprocess.PIPE, stderr=subprocess.PIPE)
            total, t0 = 0, time.time()
            # Ordered, bounded pipeline: keep WINDOW segments in flight, write
            # them to ffmpeg in playlist order as they complete.
            with ThreadPoolExecutor(max_workers=WORKERS) as pool:
                pending = []
                it = iter(segs)
                for s in it:
                    pending.append(pool.submit(fetch, s))
                    if len(pending) >= WINDOW:
                        break
                i = 0
                while pending:
                    data = pending.pop(0).result()
                    ff.stdin.write(data)
                    total += len(data)
                    i += 1
                    nxt = next(it, None)
                    if nxt is not None:
                        pending.append(pool.submit(fetch, nxt))
                    if i % 100 == 0:
                        job.update(fetched=i, bytes=total, seconds=round(time.time() - t0, 1))
            ff.stdin.close()
            err = ff.stderr.read().decode("utf8", "replace")[-500:]
            if ff.wait() != 0:
                raise RuntimeError(f"ffmpeg exit {ff.returncode}: {err}")
            os.replace(str(out) + ".part", out)
            job.update(status="ready", fetched=len(segs), bytes=total,
                       seconds=round(time.time() - t0, 1), audio_bytes=out.stat().st_size,
                       duration=dubmux.ffprobe_duration(str(out)), finished=time.time())
        except Exception as e:  # noqa: BLE001
            print(f"extract {job_id} failed: {str(e)[-400:]}", file=sys.stderr, flush=True)
            job.update(status="error", error=str(e)[-400:], finished=time.time())
            Path(str(out) + ".part").unlink(missing_ok=True)


@app.post("/extract")
async def extract(req: Request):
    body = await req.json()
    job_id, url = body.get("id") or "", body.get("url") or ""
    if not job_id.replace("_", "").replace("-", "").isalnum() or not url.startswith("http"):
        raise HTTPException(400, "id and url required")
    out = DATA / f"{job_id}.m4a"
    with _lock:
        job = _jobs.get(job_id)
        if out.exists() and (not job or job.get("status") == "ready"):
            _jobs[job_id] = job = {"status": "ready", "audio_bytes": out.stat().st_size,
                                   "cached": True, "finished": out.stat().st_mtime}
            return job
        if not job or job.get("status") == "error":
            _jobs[job_id] = job = {"status": "queued", "url": url[:80]}
            threading.Thread(target=_run, args=(job_id, url), daemon=True).start()
    _sweep()
    return job


@app.get("/extract/{job_id}")
def status(job_id: str):
    job = _jobs.get(job_id)
    if not job:
        out = DATA / f"{job_id}.m4a"
        if out.exists():
            return {"status": "ready", "audio_bytes": out.stat().st_size, "cached": True}
        raise HTTPException(404, "unknown job")
    return job


@app.get("/extract/{job_id}/audio")
def audio(job_id: str):
    out = DATA / f"{job_id}.m4a"
    if not out.exists():
        raise HTTPException(404, "not ready")
    return FileResponse(str(out), media_type="audio/mp4")


def _sweep():
    cutoff = time.time() - TTL_HOURS * 3600
    for f in DATA.glob("*.m4a*"):
        if f.stat().st_mtime < cutoff:
            f.unlink(missing_ok=True)


@app.get("/health")
def health():
    du = shutil.disk_usage(DATA)
    return {"ok": True, "jobs": {k: v.get("status") for k, v in _jobs.items()},
            "files": len(list(DATA.glob("*.m4a"))), "free_mb": du.free // 1_000_000}
