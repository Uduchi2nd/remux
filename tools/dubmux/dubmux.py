#!/usr/bin/env python3
"""dubmux phase 1: extract a Vietnamese dub audio track from a vnphim HLS
stream, cache it, and match it against a high-quality video source.

    dubmux.py extract  <name> <hls-url> [--reencode]
    dubmux.py match    <name> <video-url> [--windows 90,mid,-150] [--span 120]
    dubmux.py sample   <name> <video-url> <offset-s> [--at 600 --len 90]

Cache layout: $DUBMUX_CACHE/<name>.m4a + <name>.json (meta). Matching decodes
short mono 8 kHz windows from both sides and cross-correlates them (FFT) at
several points along the runtime; the lag must agree across windows.
"""
import argparse, json, os, re, shutil, subprocess, sys, tempfile, threading, time, urllib.request
from concurrent.futures import ThreadPoolExecutor
from urllib.parse import urljoin
import numpy as np

CACHE = os.environ.get("DUBMUX_CACHE", os.path.expanduser("~/dubmux/cache"))
RATE = 8000
UA = "Infuse-Direct/8.5.3"


RUN_TIMEOUT_S = 900   # no single ffmpeg/ffprobe may hold a gate slot for hours


def run(cmd, **kw):
    kw.setdefault("timeout", RUN_TIMEOUT_S)
    try:
        r = subprocess.run(cmd, capture_output=True, **kw)
    except subprocess.TimeoutExpired:
        raise RuntimeError(f"{os.path.basename(cmd[0])} timed out after {kw['timeout']}s")
    if r.returncode != 0:
        tail = r.stderr.decode("utf8", "replace").strip()[-600:]
        raise RuntimeError(f"{os.path.basename(cmd[0])} exit {r.returncode}: {tail}")
    return r


def ua_for(url):
    # -rw_timeout: give up on a remote read idle for 30 s (2026-09-27: three
    # pre-screen decodes hung 8 h on a stalled TorBox CDN node and held every
    # general match slot, freezing the whole queue)
    return ["-user_agent", UA, "-rw_timeout", "30000000"] if url.startswith(("http://", "https://")) else []


class SourceUnavailable(RuntimeError):
    """The resolver answered with a placeholder instead of the release
    (AIOStreams' rate-limit / error videos): retry later, never mux it."""


# AIOStreams answers a rate-limited or failed playback request with a 307 to a
# short placeholder video (`/static/429.mp4`, `/static/500.mp4`, ...). Muxing
# that produced a dub row that "loads but never plays" (2026-09-27).
# Torrentio does the same with its own clips under /videos/ (30 s
# `limits_exceeded_v2.mp4` when its per-IP limit is hit; also downloading_*,
# failed_*): those were "rejected" as a duration mismatch, i.e. permanently
# (Against the Current E1, 2026-09-30) instead of retried.
_PLACEHOLDER = re.compile(r"/static/\d{3}[^/]*\.mp4$|/static/[a-z_-]*error[^/]*\.mp4$"
                          r"|torrentio\.strem\.fun/videos/[^/]+\.mp4$"
                          r"|/videos/[a-z_]*(?:limit|exceed|fail|error|download|unavailable)[^/]*\.mp4$", re.I)
_resolved: dict = {}          # url -> (final url, time)
_resolve_lock = threading.Lock()
_last_bg_resolve = [0.0]
RESOLVE_TTL_S = 3 * 3600      # debrid CDN links stay valid for hours
BG_RESOLVE_GAP_S = float(os.environ.get("DUBMUX_BG_RESOLVE_GAP_S", "20"))


# Circuit breaker for BACKGROUND resolves, PER RESOLVER HOST (2026-09-30:
# Torrentio's per-IP limit must not pause AIOStreams/TorBox preparations, and
# vice versa): the first placeholder answer pauses that host's background
# resolves 30 min, doubling per consecutive trip up to 4 h; a real answer
# from the host resets its backoff.
_breakers: dict = {}          # host -> {"until": t, "last": pause}
BREAKER_FIRST_S, BREAKER_MAX_S = 1800, 4 * 3600


def _host(url):
    from urllib.parse import urlsplit
    return urlsplit(url or "").netloc.lower()


def breaker_remaining(url=None):
    """Seconds left on the breaker of `url`'s resolver host; without a url,
    the longest pause of any host (health/status)."""
    now = time.time()
    if url is not None:
        b = _breakers.get(_host(url))
        return max(0, int(b["until"] - now)) if b else 0
    return max([0] + [int(b["until"] - now) for b in _breakers.values()])


def _trip_breaker(url):
    b = _breakers.setdefault(_host(url), {"until": 0.0, "last": 0.0})
    if time.time() < b["until"]:
        return
    pause = BREAKER_FIRST_S if not b["last"] else min(b["last"] * 2, BREAKER_MAX_S)
    b.update(until=time.time() + pause, last=pause)
    print(f"  resolver rate-limited ({_host(url)}): background resolves paused {pause // 60} min",
          file=sys.stderr, flush=True)


def resolve_url(url, timeout=20, background=False):
    """Follow addon/debrid redirectors (AIOStreams playback URLs, Torrentio
    resolvers, TorBox API) to the final CDN URL. ffmpeg can follow redirects
    but cannot seek through them, and the muxer must not hammer a resolver
    once per segment window anyway. Results are cached (a seek or a producer
    restart never asks the resolver again); background callers are paced so
    they cannot trip the resolver's rate limit; placeholders raise."""
    if not url.startswith(("http://", "https://")):
        return url
    now = time.time()
    hit = _resolved.get(url)
    if hit and now - hit[1] < RESOLVE_TTL_S:
        return hit[0]
    if background:
        if breaker_remaining(url):
            raise SourceUnavailable(f"background resolves paused {breaker_remaining(url)}s "
                                    f"({_host(url)} rate limit)")
        with _resolve_lock:
            gap = _last_bg_resolve[0] + BG_RESOLVE_GAP_S - time.time()
            if gap > 0:
                time.sleep(gap)
            _last_bg_resolve[0] = time.time()
    req = urllib.request.Request(url, headers={"User-Agent": UA, "Range": "bytes=0-0"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            final = r.geturl()
    except urllib.error.HTTPError as e:
        # 416 etc. still carries the final URL after redirects.
        final = e.geturl() or url
    except Exception:  # noqa: BLE001
        return url
    if _PLACEHOLDER.search(final.split("?")[0]):
        _trip_breaker(url)
        raise SourceUnavailable(f"resolver returned a placeholder ({final.rsplit('/', 1)[-1]})")
    b = _breakers.get(_host(url))
    if b and time.time() >= b["until"]:
        b["last"] = 0.0                 # answering normally again: reset the backoff
    if final != url:
        _resolved[url] = (final, now)
        if len(_resolved) > 5000:
            for k in sorted(_resolved, key=lambda k: _resolved[k][1])[:1000]:
                _resolved.pop(k, None)
    return final


_codecs: dict = {}


def video_codec(url):
    """Codec name of the first video stream (cached per URL), or None."""
    if url in _codecs:
        return _codecs[url]
    try:
        out = run(["ffprobe", "-v", "error", *ua_for(url), "-select_streams", "v:0", "-show_entries",
                   "stream=codec_name", "-of", "csv=p=0", url], timeout=120).stdout.decode().strip()
    except Exception:  # noqa: BLE001
        return None
    _codecs[url] = out.split(",")[0].strip() or None
    return _codecs[url]


# Rewrite the video's parameter sets from the stream itself before packaging
# into MPEG-TS. Some releases (Amazon HDR10+/Dolby Vision HEVC, e.g. Reacher)
# carry DIFFERENT parameter sets in the container header than in-band; the
# MPEG-TS muxer's automatic mp4->Annex-B conversion then inserts the header
# set before every keyframe and the decoder produces green/black corruption
# (97 errors in 30 s; 0 with this rewrite; fMP4/MKV were unaffected).
_REWRITE = {"hevc": "hevc_metadata", "h264": "h264_metadata"}


def ts_video_bsf(url):
    bsf = _REWRITE.get(video_codec(url) or "")
    return ["-bsf:v", bsf] if bsf else []


def ffprobe_duration(url):
    out = run(["ffprobe", "-v", "error", *ua_for(url), "-show_entries",
               "format=duration", "-of", "csv=p=0", url]).stdout.decode().strip()
    return float(out)


def ffprobe_audio(url):
    out = run(["ffprobe", "-v", "error", *ua_for(url), "-select_streams", "a",
               "-show_entries", "stream=codec_name,channels,sample_rate",
               "-of", "json", url]).stdout
    return json.loads(out).get("streams", [])


def decode(url, start, span, extra=()):
    """Mono float32 @ RATE for [start, start+span)."""
    cmd = ["ffmpeg", "-v", "error", "-nostdin", *ua_for(url), *extra,
           "-ss", f"{start:.3f}", "-t", f"{span:.3f}", "-i", url, "-vn", "-sn",
           "-ac", "1", "-ar", str(RATE), "-f", "f32le", "-"]
    pcm = run(cmd).stdout
    return np.frombuffer(pcm, dtype=np.float32)


def prescreen(video_url, dubfile, vdur, ddur, n=8, span=60.0, max_lag=75.0, min_ratio=10.0):
    """Cheap decision before any full read of the release: correlate `n`
    60 s windows spread over the file (byte-range seeks, ~30 MB each of
    interleaved data) against the dub at the same nominal time. Returns
    [(t, lag, ratio)] — the caller counts confident windows (ratio >=
    min_ratio) and looks at lag agreement."""
    from concurrent.futures import ThreadPoolExecutor
    lim = min(vdur, ddur) - span - 5
    if lim <= span:
        return []
    ts = [round(lim * (0.06 + 0.88 * i / (n - 1)), 1) for i in range(n)]

    def one(t):
        try:
            a = decode(video_url, t, span)
            b = decode(dubfile, t, span)
            m = min(len(a), len(b))
            if m < int(span * RATE * 0.8):
                return (t, None, 0.0)
            lag, _peak, ratio = xcorr_lag(a[:m], b[:m], max_lag)
            return (t, round(float(lag), 3), round(float(ratio), 1))
        except Exception:  # noqa: BLE001
            return (t, None, 0.0)
    with ThreadPoolExecutor(max_workers=4) as pool:
        return list(pool.map(one, ts))


def xcorr_lag(a, b, max_lag_s):
    """Lag (seconds) such that a(t) ≈ b(t + lag): with a = video and b = dub,
    video(t) lines up with dub(t + lag). Negative lag = the video has extra
    content before the point where the dub starts. (Verified with a synthetic
    2 s head: lag = -2.000.)"""
    n = min(len(a), len(b))
    a = a[:n] - a[:n].mean(); b = b[:n] - b[:n].mean()
    a /= (np.linalg.norm(a) or 1); b /= (np.linalg.norm(b) or 1)
    size = 1 << (2 * n - 1).bit_length()
    corr = np.fft.irfft(np.fft.rfft(b, size) * np.conj(np.fft.rfft(a, size)), size)
    corr = np.concatenate((corr[-(n - 1):], corr[:n]))  # lags -(n-1)..(n-1)
    lags = np.arange(-(n - 1), n)
    m = int(max_lag_s * RATE)
    keep = (lags >= -m) & (lags <= m)
    corr, lags = corr[keep], lags[keep]
    i = int(np.argmax(corr))
    peak = float(corr[i])
    noise = float(np.sqrt(np.mean(corr ** 2))) or 1e-9
    return lags[i] / RATE, peak, peak / noise


def envelope(x, win=80):
    e = np.abs(x)
    k = np.ones(win, dtype=np.float32) / win
    return np.convolve(e, k, mode="same")


def http_get(url, rng=None, timeout=60, tries=4):
    hdr = {"User-Agent": UA}
    if rng:
        hdr["Range"] = f"bytes={rng[0]}-{rng[0] + rng[1] - 1}"
    last = None
    for attempt in range(tries):
        try:
            with urllib.request.urlopen(urllib.request.Request(url, headers=hdr), timeout=timeout) as r:
                data = r.read()
            if rng and len(data) != rng[1]:
                raise IOError(f"short range read {len(data)} != {rng[1]}")
            return data
        except Exception as e:  # noqa: BLE001
            last = e
            time.sleep(0.5 * (attempt + 1))
    raise last


def _join(url, rel):
    """urljoin that understands MediaFlow `/proxy/stream?d=<origin>&…` wraps:
    a relative URI in a wrapped playlist is relative to the ORIGIN, and the
    result is wrapped again with the same credentials/headers."""
    if rel.startswith(("http://", "https://")):
        return rel
    if "/proxy/stream?" in url:
        from urllib.parse import parse_qsl, urlencode, urlsplit, urlunsplit
        parts = urlsplit(url)
        q = parse_qsl(parts.query, keep_blank_values=True)
        d = dict(q).get("d")
        if d:
            q = [(k, urljoin(d, rel) if k == "d" else v) for k, v in q]
            return urlunsplit((parts.scheme, parts.netloc, parts.path, urlencode(q), ""))
    return urljoin(url, rel)


def parse_media_playlist(url):
    """Follow a master to its first variant; return (variant_url, [(seg_url, byterange|None)])."""
    text = http_get(url).decode("utf8", "replace")
    if "#EXT-X-STREAM-INF" in text:
        variant = next(l for l in text.splitlines() if l and not l.startswith("#"))
        url = _join(url, variant)
        text = http_get(url).decode("utf8", "replace")
    segs, br = [], None
    for line in text.splitlines():
        if line.startswith("#EXT-X-BYTERANGE:"):
            n, o = line[17:].split("@")
            br = (int(o), int(n))
        elif line and not line.startswith("#"):
            segs.append((_join(url, line), br))
            br = None
    if not segs:
        raise RuntimeError("no segments in playlist")
    return url, segs


def parallel_fetch_ts(url, workers, log=sys.stderr):
    """Download every segment concurrently, write them in playlist order to a
    temp .ts, return (path, bytes, seconds). Order is preserved by index, so
    the concatenation is the same byte stream ffmpeg would have produced
    sequentially — just N connections wide."""
    _, segs = parse_media_playlist(url)
    # Proxied (geo-blocked) playlists route every segment through the VN
    # MediaFlow on a home uplink: a wide fan-out there just times out and
    # hurts real viewers. Keep those narrow; open CDNs get the full width.
    if any("mediaflow" in s[0] or "/proxy/stream" in s[0] for s in segs[:3]):
        workers = min(workers, 6)
    t0 = time.time()
    tmp = tempfile.NamedTemporaryFile(prefix="dubmux-", suffix=".ts", delete=False, dir=CACHE)
    total = 0
    with ThreadPoolExecutor(max_workers=workers) as pool:
        # map() yields results in submission order while downloads run ahead.
        for i, data in enumerate(pool.map(lambda s: http_get(s[0], s[1]), segs)):
            tmp.write(data)
            total += len(data)
            if i % 100 == 0 or i == len(segs) - 1:
                print(f"  fetched {i + 1}/{len(segs)} ({total / 1e6:.0f} MB, {time.time() - t0:.1f}s)",
                      file=log, flush=True)
    tmp.close()
    return tmp.name, total, time.time() - t0


VN_EXTRACTOR = os.environ.get("DUBMUX_VN_EXTRACTOR", "").rstrip("/")


def is_proxied_playlist(url):
    """vnphim `/p/s.` `/p/D.` playlists: segments go through the VN MediaFlow."""
    try:
        _, segs = parse_media_playlist(url)
    except Exception:  # noqa: BLE001
        return False
    return any("/proxy/stream" in s[0] or "mediaflow" in s[0] for s in segs[:3])


def vn_extract(name, url, out, log=sys.stderr, timeout=1500):
    """Have the VN-side extractor (LXC 100 next to MediaFlow) pull the origin
    segments with domestic bandwidth and demux the AAC there; only the ~70 MB
    result crosses the VN uplink. Returns fetch stats or raises."""
    # The seedbox runs tailscaled in userspace mode: tailnet hosts are only
    # reachable through its local HTTP CONNECT proxy, which also resolves
    # MagicDNS names (the VN tailscale-serve cert is for that name).
    proxy = os.environ.get("DUBMUX_VN_PROXY", "http://127.0.0.1:1055")
    opener = urllib.request.build_opener(
        urllib.request.ProxyHandler({"http": proxy, "https": proxy} if proxy else {}))
    body = json.dumps({"id": name, "url": url}).encode()
    def call(path, data=None):
        req = urllib.request.Request(VN_EXTRACTOR + path, data=data,
                                     headers={"Content-Type": "application/json"})
        with opener.open(req, timeout=60) as r:
            return r.read()
    job = json.loads(call("/extract", body))
    t0 = time.time()
    while job.get("status") in ("queued", "running"):
        if time.time() - t0 > timeout:
            raise RuntimeError("vn extractor timed out")
        time.sleep(5)
        job = json.loads(call(f"/extract/{name}"))
        print(f"  vn extract: {job.get('status')} {job.get('fetched', 0)}/{job.get('segments', '?')} "
              f"{(job.get('bytes') or 0) / 1e6:.0f} MB", file=log, flush=True)
    if job.get("status") != "ready":
        raise RuntimeError(f"vn extractor: {job.get('status')} {job.get('error', '')}")
    req = urllib.request.Request(VN_EXTRACTOR + f"/extract/{name}/audio")
    with opener.open(req, timeout=600) as r, open(out + ".part", "wb") as f:
        shutil.copyfileobj(r, f, 1 << 20)
    os.replace(out + ".part", out)
    return {"segments_bytes": job.get("bytes"), "fetch_seconds": job.get("seconds"),
            "workers": "vn-extractor", "transfer_seconds": round(time.time() - t0, 1)}


def vn_opener():
    """urllib opener for the VN extractor (seedbox tailscaled is userspace:
    tailnet hosts only through its local CONNECT proxy)."""
    proxy = os.environ.get("DUBMUX_VN_PROXY", "http://127.0.0.1:1055")
    return urllib.request.build_opener(
        urllib.request.ProxyHandler({"http": proxy, "https": proxy} if proxy else {}))


def vn_sizes(urls, timeout=300):
    """Origin byte sizes of segment URLs (MediaFlow wraps or plain origins),
    measured in VN (kkphim/ophim origins are geo-blocked abroad)."""
    out = []
    for i in range(0, len(urls), 1000):
        req = urllib.request.Request(VN_EXTRACTOR + "/sizes", data=json.dumps({"urls": urls[i:i + 1000]}).encode(),
                                     headers={"Content-Type": "application/json"})
        with vn_opener().open(req, timeout=timeout) as r:
            out += json.loads(r.read())["sizes"]
    return out


def vn_segment(url, timeout=120):
    """One origin segment, fetched in VN and relayed over the tailnet."""
    from urllib.parse import quote
    req = urllib.request.Request(VN_EXTRACTOR + "/seg?u=" + quote(url, safe=""))
    with vn_opener().open(req, timeout=timeout) as r:
        return r.read()


VNPHIM_URL = os.environ.get("DUBMUX_VNPHIM_URL", "").rstrip("/")
VNPHIM_KEY = os.environ.get("DUBMUX_VNPHIM_KEY", "")
MF_PASSWORD = os.environ.get("DUBMUX_MF_PASSWORD", "")


def dub_source(url, log=sys.stderr):
    """A vnphim stream address is an encrypted MediaFlow URL (2026-09-26) that
    nobody can decode. Ask vnphim what it stands for and rebuild a fetchable
    address: the VN extractor gets a MediaFlow-style wrap (origin + h_Referer,
    which it unwraps), the seedbox fallback the same wrap with the password.
    Falls back to the URL itself when vnphim does not know it."""
    # a vnphim seekable-file stream (/vn/<id>/file.ts, Americas viewers) is
    # asked for too: vnphim answers for the playlist behind it
    is_file = "/vn/" in url and "/file.ts" in url
    if ("_token_" not in url and not is_file) or not (VNPHIM_URL and VNPHIM_KEY):
        return url
    from urllib.parse import quote, urlencode
    req = urllib.request.Request(f"{VNPHIM_URL}/_internal/dub-source?u={quote(url, safe='')}",
                                 headers={"X-Vnphim-Key": VNPHIM_KEY, "User-Agent": UA})
    try:
        with urllib.request.urlopen(req, timeout=20) as r:
            info = json.loads(r.read())
    except Exception as e:  # noqa: BLE001
        print(f"  dub source lookup failed ({e}); using the stream url", file=log, flush=True)
        return url
    origin = info.get("origin")
    if not origin:
        return url
    if info.get("direct") or origin.startswith(VNPHIM_URL):
        return origin  # /y/, /h/ or a directly reachable playlist: fetch as-is
    mf = (info.get("mediaflow") or "").rstrip("/") or url.split("/_token_")[0]
    if is_file and not info.get("mediaflow"):
        return url
    params = [("d", origin)]
    if MF_PASSWORD:
        params.append(("api_password", MF_PASSWORD))
    if info.get("referer"):
        params.append(("h_Referer", info["referer"]))
    wrapped = f"{mf}/proxy/stream?{urlencode(params)}"
    print(f"  dub source: {origin[:70]} (via vnphim lookup)", file=log, flush=True)
    return wrapped


def cmd_extract(args):
    os.makedirs(CACHE, exist_ok=True)
    out = os.path.join(CACHE, args.name + ".m4a")
    meta = os.path.join(CACHE, args.name + ".json")
    t0 = time.time()
    fetch = None
    args.url = dub_source(args.url)
    src = args.url
    if VN_EXTRACTOR and is_proxied_playlist(args.url):
        try:
            fetch = vn_extract(args.name, args.url, out)
            dur = ffprobe_duration(out)
            info = {"name": args.name, "source": args.url, "codec_in": "aac", "copied": True,
                    "duration": dur, "bytes": os.path.getsize(out), "created": int(time.time()),
                    "extract_seconds": round(time.time() - t0, 1), "fetch": fetch}
            json.dump(info, open(meta, "w"), indent=1)
            print(json.dumps(info, indent=1))
            return
        except Exception as e:  # noqa: BLE001
            print(f"  vn extractor unavailable ({str(e)[-200:]}); extracting via the proxy",
                  file=sys.stderr, flush=True)
    try:
        if args.parallel > 1:
            ts_path, ts_bytes, fetch_s = parallel_fetch_ts(args.url, args.parallel)
            fetch = {"segments_bytes": ts_bytes, "fetch_seconds": round(fetch_s, 1),
                     "workers": args.parallel}
            src = ts_path
        streams = ffprobe_audio(src)
        codec = streams[0]["codec_name"] if streams else None
        if codec == "aac" and not args.reencode:
            acodec = ["-c:a", "copy"]
        else:
            acodec = ["-c:a", "aac", "-b:a", "160k"]
        # -copyts + aresample=async keeps timeline holes (stripped ad clusters)
        # as silence instead of collapsing them, so the track stays on the
        # origin's clock. Only meaningful when re-encoding.
        filt = ["-af", "aresample=async=1:first_pts=0"] if acodec[1] != "copy" else []
        run(["ffmpeg", "-v", "error", "-nostdin", "-y", *ua_for(src),
             "-i", src, "-vn", "-sn", "-map", "0:a:0", *filt, *acodec,
             "-movflags", "+faststart", "-f", "ipod", out + ".part"])
        os.replace(out + ".part", out)
    finally:
        if src != args.url and os.path.exists(src):
            os.unlink(src)
        if os.path.exists(out + ".part"):
            os.unlink(out + ".part")
    dur = ffprobe_duration(out)
    info = {"name": args.name, "source": args.url, "codec_in": codec,
            "copied": acodec[1] == "copy", "duration": dur,
            "bytes": os.path.getsize(out), "created": int(time.time()),
            "extract_seconds": round(time.time() - t0, 1), "fetch": fetch}
    json.dump(info, open(meta, "w"), indent=1)
    print(json.dumps(info, indent=1))


def cmd_match(args):
    dub = os.path.join(CACHE, args.name + ".m4a")
    meta = json.load(open(os.path.join(CACHE, args.name + ".json")))
    vdur = ffprobe_duration(args.video)
    ddur = meta["duration"]
    report = {"name": args.name, "video": args.video[:80], "video_duration": vdur,
              "dub_duration": ddur, "duration_delta": round(vdur - ddur, 3)}
    if abs(vdur - ddur) > args.tolerance:
        report["verdict"] = "reject:duration"
        print(json.dumps(report, indent=1)); return 1
    windows = []
    for w in args.windows.split(","):
        w = w.strip()
        if w == "mid": start = vdur / 2
        elif w.startswith("-"): start = vdur + float(w) - args.span
        else: start = float(w)
        windows.append(max(0.0, min(start, vdur - args.span - 1)))
    t0 = time.time()

    def one(start):
        a = decode(args.video, start, args.span)
        b = decode(dub, start, args.span)
        lag, peak, ratio = xcorr_lag(a, b, args.max_lag)
        method = "waveform"
        if ratio < 6:
            lag2, peak2, ratio2 = xcorr_lag(envelope(a), envelope(b), args.max_lag)
            if ratio2 > ratio:
                lag, peak, ratio, method = lag2, peak2, ratio2, "envelope"
        print(f"  window@{start:7.1f}s lag={lag:+.3f}s peak/rms={ratio:.1f} ({method}) "
              f"[{time.time() - t0:.1f}s]", file=sys.stderr, flush=True)
        return {"start": round(start, 1), "lag_s": round(float(lag), 3),
                "peak": round(peak, 4), "peak_to_rms": round(ratio, 1), "method": method}

    # Each window is an independent HTTP seek + decode; run them together.
    with ThreadPoolExecutor(max_workers=len(windows)) as pool:
        results = list(pool.map(one, windows))
    report["match_seconds"] = round(time.time() - t0, 1)
    lags = np.array([r["lag_s"] for r in results])
    report["windows"] = results
    report["lag_spread"] = round(float(lags.max() - lags.min()), 3)
    report["lag_median"] = round(float(np.median(lags)), 3)
    ok = all(r["peak_to_rms"] >= args.min_ratio for r in results) and \
        report["lag_spread"] <= args.max_spread
    report["verdict"] = "accept" if ok else "reject:correlation"
    print(json.dumps(report, indent=1))
    return 0 if ok else 2


def cmd_sample(args):
    dub = os.path.join(CACHE, args.name + ".m4a")
    out = args.out or os.path.join(CACHE, f"{args.name}-sample-{int(args.at)}.mp4")
    # video(t) <-> dub(t + lag)  =>  when cutting video at T, cut dub at T + lag.
    run(["ffmpeg", "-v", "error", "-nostdin", "-y", *ua_for(args.video),
         "-ss", f"{args.at:.3f}", "-t", f"{args.len:.3f}", "-i", args.video,
         "-ss", f"{args.at + args.offset:.3f}", "-t", f"{args.len:.3f}", "-i", dub,
         "-map", "0:v:0", "-map", "1:a:0", "-map", "0:a", "-c", "copy",
         "-metadata:s:a:0", "language=vie", "-metadata:s:a:0", "title=Thuyết Minh (vnphim)",
         "-disposition:a:0", "default", "-disposition:a:1", "0",
         "-movflags", "+faststart", out])
    print(out, os.path.getsize(out))


def main():
    p = argparse.ArgumentParser()
    sub = p.add_subparsers(dest="cmd", required=True)
    e = sub.add_parser("extract"); e.add_argument("name"); e.add_argument("url")
    e.add_argument("--reencode", action="store_true")
    e.add_argument("--parallel", type=int, default=48, help="segment download workers (1 = let ffmpeg stream it)")
    e.set_defaults(fn=cmd_extract)
    m = sub.add_parser("match"); m.add_argument("name"); m.add_argument("video")
    m.add_argument("--windows", default="90,mid,-150"); m.add_argument("--span", type=float, default=120)
    m.add_argument("--tolerance", type=float, default=1.5); m.add_argument("--max-lag", type=float, default=60)
    m.add_argument("--min-ratio", type=float, default=8); m.add_argument("--max-spread", type=float, default=0.15)
    m.set_defaults(fn=cmd_match)
    s = sub.add_parser("sample"); s.add_argument("name"); s.add_argument("video")
    s.add_argument("offset", type=float); s.add_argument("--at", type=float, default=600)
    s.add_argument("--len", type=float, default=90); s.add_argument("--out"); s.set_defaults(fn=cmd_sample)
    args = p.parse_args()
    sys.exit(args.fn(args) or 0)


if __name__ == "__main__":
    main()
