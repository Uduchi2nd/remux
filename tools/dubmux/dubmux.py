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
import argparse, json, os, re, shutil, subprocess, sys, tempfile, time, urllib.request
from concurrent.futures import ThreadPoolExecutor
from urllib.parse import urljoin
import numpy as np

CACHE = os.environ.get("DUBMUX_CACHE", os.path.expanduser("~/dubmux/cache"))
RATE = 8000
UA = "Infuse-Direct/8.5.3"


def run(cmd, **kw):
    r = subprocess.run(cmd, capture_output=True, **kw)
    if r.returncode != 0:
        tail = r.stderr.decode("utf8", "replace").strip()[-600:]
        raise RuntimeError(f"{os.path.basename(cmd[0])} exit {r.returncode}: {tail}")
    return r


def ua_for(url):
    return ["-user_agent", UA] if url.startswith(("http://", "https://")) else []


def resolve_url(url, timeout=20):
    """Follow addon/debrid redirectors (AIOStreams playback URLs, Torrentio
    resolvers, TorBox API) to the final CDN URL. ffmpeg can follow redirects
    but cannot seek through them, and the muxer must not hammer a resolver
    once per segment window anyway."""
    if not url.startswith(("http://", "https://")):
        return url
    req = urllib.request.Request(url, headers={"User-Agent": UA, "Range": "bytes=0-0"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.geturl()
    except urllib.error.HTTPError as e:
        # 416 etc. still carries the final URL after redirects.
        return e.geturl() or url
    except Exception:  # noqa: BLE001
        return url


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


def parse_media_playlist(url):
    """Follow a master to its first variant; return (variant_url, [(seg_url, byterange|None)])."""
    text = http_get(url).decode("utf8", "replace")
    if "#EXT-X-STREAM-INF" in text:
        variant = next(l for l in text.splitlines() if l and not l.startswith("#"))
        url = urljoin(url, variant)
        text = http_get(url).decode("utf8", "replace")
    segs, br = [], None
    for line in text.splitlines():
        if line.startswith("#EXT-X-BYTERANGE:"):
            n, o = line[17:].split("@")
            br = (int(o), int(n))
        elif line and not line.startswith("#"):
            segs.append((urljoin(url, line), br))
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


VNPHIM_URL = os.environ.get("DUBMUX_VNPHIM_URL", "").rstrip("/")
VNPHIM_KEY = os.environ.get("DUBMUX_VNPHIM_KEY", "")
MF_PASSWORD = os.environ.get("DUBMUX_MF_PASSWORD", "")


def dub_source(url, log=sys.stderr):
    """A vnphim stream address is an encrypted MediaFlow URL (2026-09-26) that
    nobody can decode. Ask vnphim what it stands for and rebuild a fetchable
    address: the VN extractor gets a MediaFlow-style wrap (origin + h_Referer,
    which it unwraps), the seedbox fallback the same wrap with the password.
    Falls back to the URL itself when vnphim does not know it."""
    if "_token_" not in url or not (VNPHIM_URL and VNPHIM_KEY):
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
    if origin.startswith(VNPHIM_URL):
        return origin  # /y/ or /h/: open segments, fetch as-is
    mf = url.split("/_token_")[0]
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
