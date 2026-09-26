"""Piecewise alignment of a Vietnamese dub onto a differently-cut release.

VN encodes (kkphim/ophim) are usually the same episode minus a few blocks —
the streamer ident, the opening credits, the tail preview — so the lag
between the dub and the HQ video is piecewise constant: one value per run,
jumping wherever the HQ has content the dub lacks. With
`video(t) <-> dub(t + lag)` (see dubmux.xcorr_lag) a run with lag L covering
dub time [d0, d1) covers video time [d0 - L, d1 - L).

The HQ audio is fetched ONCE (`fetch_hq_audio`, stream-copied into a local
.mka): debrid CDNs rate-limit dozens of ranged seeks, and the whole track is
needed anyway to fill the gaps with the release's own audio. Everything
else works on local files / in-memory PCM.
"""
import json, os, subprocess

import numpy as np

import dubmux

COARSE_STEP = 60.0
COARSE_SPAN = 60.0      # 30 s windows produced spurious single-window runs
FINE_SPAN = 10.0
FINE_STEP = 2.0
MIN_RATIO = 12.0        # coarse window confidence floor (peak / rms)
STRONG_RATIO = 30.0     # a lone window needs this to stand as its own run
RUN_TOL = 0.25          # lags within this are the same run
MAX_RUNS = 8
MAX_SKEW = 180.0        # give up beyond this much total difference
MIN_COVERAGE = 0.85     # dub time / windows that must be covered by accepted runs
MAX_GAP_WINDOWS = 4     # inside a run: up to 4 unconfident windows (240 s) between confident ones
R = dubmux.RATE


def fetch_hq_audio(video_url, out_path):
    """Stream-copy the release's first audio track into a local Matroska
    audio file (any codec). One connection, sequential read."""
    if os.path.exists(out_path):
        return out_path
    dubmux.run(["ffmpeg", "-nostdin", "-y", "-v", "error", *dubmux.ua_for(video_url),
                "-i", video_url, "-vn", "-sn", "-map", "0:a:0", "-c:a", "copy",
                "-f", "matroska", out_path + ".part"])
    os.replace(out_path + ".part", out_path)
    return out_path


def decode_all(path):
    """Whole file as mono float32 @ R (about 32 kB per second)."""
    pcm = dubmux.run(["ffmpeg", "-v", "error", "-nostdin", "-i", path, "-vn", "-sn",
                      "-ac", "1", "-ar", str(R), "-f", "f32le", "-"]).stdout
    return np.frombuffer(pcm, dtype=np.float32)


def _lag(video_pcm, dub_pcm, start, span, max_lag):
    """Correlate the window [start, start+span) taken at the same nominal
    time on both streams; the correlation absorbs the offset."""
    i0, i1 = int(start * R), int((start + span) * R)
    a, b = video_pcm[i0:i1], dub_pcm[i0:i1]
    n = min(len(a), len(b))
    if n < int(span * R * 0.8):
        return None
    lag, _peak, ratio = dubmux.xcorr_lag(a[:n], b[:n], max_lag)
    return float(lag), float(ratio)


def _coef(video_pcm, dub_pcm, start, span, lag):
    """Normalised correlation of dub[start, start+span) against the video at
    the fixed lag (video(t) <-> dub(t + lag)); no search, so short windows
    are fine. Used to pin a cut between two known lags."""
    d0, d1 = int(start * R), int((start + span) * R)
    v0 = int((start - lag) * R)
    if v0 < 0 or d0 < 0:
        return 0.0
    b = dub_pcm[d0:d1]
    a = video_pcm[v0:v0 + len(b)]
    n = min(len(a), len(b))
    if n < span * R * 0.5:
        return 0.0
    a = a[:n] - a[:n].mean(); b = b[:n] - b[:n].mean()
    den = (np.linalg.norm(a) * np.linalg.norm(b)) or 1e-9
    return float(np.dot(a, b) / den)


def refine_cut(video_pcm, dub_pcm, lo, hi, lag_a, lag_b, span=3.0, step=0.5):
    """Earliest dub time in [lo, hi] from which lag_b explains the audio
    better than lag_a, to `step` precision."""
    best = hi
    for c in _frange(lo, hi + 1e-6, step):
        if _coef(video_pcm, dub_pcm, c, span, lag_b) > _coef(video_pcm, dub_pcm, c, span, lag_a):
            best = c
            break
    return round(best, 3)


def _frange(start, stop, step):
    x = start
    while x < stop:
        yield round(x, 3)
        x += step


def analyse(video_pcm, dub_pcm):
    """Return {"verdict", "coverage", "runs": [{dub_start, dub_end, lag,
    video_start, video_end, min_ratio, windows}]} — times in seconds."""
    vdur, ddur = len(video_pcm) / R, len(dub_pcm) / R
    skew = abs(vdur - ddur)
    if skew > MAX_SKEW:
        return {"verdict": "reject:skew", "skew": round(skew, 1)}
    max_lag = skew + 15.0
    limit = min(vdur, ddur) - COARSE_SPAN - 1
    starts = list(_frange(10.0, limit, COARSE_STEP))
    if len(starts) < 3:
        return {"verdict": "reject:short"}
    coarse = [(s, _lag(video_pcm, dub_pcm, s, COARSE_SPAN, max_lag)) for s in starts]
    points = [(s, r[0], r[1]) for s, r in coarse if r and r[1] >= MIN_RATIO]
    if len(points) < 3:
        return {"verdict": "reject:correlation",
                "coarse": [(s, r and round(r[0], 3), r and round(r[1], 1)) for s, r in coarse]}
    runs = []
    for s, lag, ratio in points:
        if runs and abs(lag - runs[-1]["lag_med"]) <= RUN_TOL:
            r = runs[-1]
            r["last"] = s
            r["lags"].append(lag)
            r["ratios"].append(ratio)
            r["starts"].append(s)
            r["lag_med"] = sorted(r["lags"])[len(r["lags"]) // 2]
        else:
            runs.append({"first": s, "last": s, "lags": [lag], "ratios": [ratio], "starts": [s], "lag_med": lag})
    # A single window at some odd lag is noise, not a cut: drop it unless it
    # is very strong, then re-merge neighbours that now agree.
    kept = [r for r in runs if len(r["lags"]) >= 2 or max(r["ratios"]) >= STRONG_RATIO]
    runs = []
    for r in kept:
        if runs and abs(r["lag_med"] - runs[-1]["lag_med"]) <= RUN_TOL:
            p = runs[-1]
            p["last"] = r["last"]
            p["lags"] += r["lags"]
            p["ratios"] += r["ratios"]
            p["starts"] += r["starts"]
            p["lag_med"] = sorted(p["lags"])[len(p["lags"]) // 2]
        else:
            runs.append(r)
    if not runs:
        return {"verdict": "reject:correlation",
                "coarse": [(s, r and round(r[0], 3), r and round(r[1], 1)) for s, r in coarse]}
    if len(runs) > MAX_RUNS:
        return {"verdict": "reject:fragmented", "runs": len(runs),
                "coarse": [(s, r and round(r[0], 3), r and round(r[1], 1)) for s, r in coarse]}
    for r in runs:
        r["lag"] = round(r["lag_med"], 3)
    # Refine each boundary: fine windows between the last confident window
    # of run i and the first of run i+1; the cut is where the lag flips.
    for i in range(len(runs) - 1):
        a, b = runs[i], runs[i + 1]
        cut = b["first"]
        for s in _frange(a["last"], b["first"], FINE_STEP):
            r = _lag(video_pcm, dub_pcm, s, FINE_SPAN, max_lag)
            if r and r[1] >= 10.0 and abs(r[0] - b["lag"]) <= RUN_TOL:
                cut = s
                break
        # `cut` is the start of the first whole fine window on the new lag;
        # the real cut can be up to FINE_SPAN earlier. Pin it to half a
        # second by comparing the two known lags directly.
        cut = refine_cut(video_pcm, dub_pcm, max(a["last"], cut - FINE_SPAN - 2), cut, a["lag"], b["lag"])
        a["dub_end"] = cut
        b["dub_start"] = cut
    # First run starts where the dub starts (video may have a longer head:
    # lag < 0 means video(t) <-> dub(t + lag), i.e. dub 0 <-> video -lag).
    runs[0]["dub_start"] = 0.0
    last_evidence = runs[-1]["last"] + COARSE_SPAN
    if min(ddur, vdur + runs[-1]["lag"]) - last_evidence <= 2 * COARSE_STEP:
        runs[-1]["dub_end"] = min(ddur, vdur + runs[-1]["lag"])
    else:
        runs[-1]["dub_end"] = last_evidence
    out = []
    for r in runs:
        rr = {"dub_start": round(max(0.0, r["dub_start"]), 3), "dub_end": round(r["dub_end"], 3),
              "lag": r["lag"], "min_ratio": round(min(r["ratios"]), 1), "windows": len(r["lags"])}
        rr["video_start"] = round(rr["dub_start"] - rr["lag"], 3)
        rr["video_end"] = round(rr["dub_end"] - rr["lag"], 3)
        if rr["dub_end"] - rr["dub_start"] > 1.0 and rr["video_start"] >= -0.5:
            out.append(rr)
    # Coverage is judged on CONFIDENT windows, not on run extent: a run's
    # last window is where the evidence ends, and the tail after it is only
    # extended to the dub's end when the evidence reaches close to the end.
    # (Pursuit of Jade vs the DDHDTV 2160p pack: 3 strong windows at -34.8 s,
    # then 43 windows of noise — the release's audio does not match past the
    # opening; that must be a rejection, not a constant offset.)
    confident = sum(1 for _s, r in coarse if r and r[1] >= MIN_RATIO)
    in_runs = sum(r["windows"] for r in out)
    coverage = in_runs / len(coarse) if coarse else 0.0
    covered = sum(r["dub_end"] - r["dub_start"] for r in out)
    verdict = "accept" if out and coverage >= MIN_COVERAGE else "reject:coverage"
    # Second criterion — TIME coverage by well-anchored runs. Dialogue-sparse
    # stretches (music, effects the dub re-mixed) correlate with nothing, so
    # their windows are "unconfident" even though the offset is constant
    # across them; a run whose confident windows bracket such a gap is still
    # trustworthy. Accept when the runs span >= MIN_COVERAGE of the dub, every
    # run has >= 2 confident windows, and no gap inside a run exceeds
    # MAX_GAP_WINDOWS (a small cut inside a longer gap would have split the
    # run). Queen of News E03 vs the DDHDTV 4K: 25/42 windows confident but
    # runs cover 94 % of the timeline with the biggest gap 3 windows.
    coverage_mode = "windows"
    if verdict != "accept" and out and ddur:
        time_cov = covered / ddur
        anchored = all(r["windows"] >= 2 for r in out)
        max_gap = 0
        for r in runs:
            ws = sorted(r["starts"]) if "starts" in r else []
            for a, b in zip(ws, ws[1:]):
                max_gap = max(max_gap, int(round((b - a) / COARSE_STEP)) - 1)
        if time_cov >= MIN_COVERAGE and anchored and max_gap <= MAX_GAP_WINDOWS:
            verdict, coverage_mode = "accept", "time"
            coverage = round(time_cov, 3)
    return {"verdict": verdict, "coverage": round(coverage, 3), "coverage_mode": coverage_mode,
            "confident_windows": confident,
            "total_windows": len(coarse), "covered_seconds": round(covered, 1), "runs": out,
            "video_duration": round(vdur, 3), "dub_duration": round(ddur, 3),
            "max_lag": max_lag,
            "coarse": [(s, r and round(r[0], 3), r and round(r[1], 1)) for s, r in coarse]}


def pieces_for(runs, vdur):
    """Video-timeline pieces: ('dub', d0, d1) or ('orig', v0, v1)."""
    pieces, cursor = [], 0.0
    for r in sorted(runs, key=lambda r: r["video_start"]):
        vs, ve = max(r["video_start"], cursor), r["video_end"]
        if ve <= vs:
            continue
        if vs > cursor + 0.05:
            pieces.append(("orig", cursor, vs))
        ds = r["dub_start"] + (vs - r["video_start"])
        pieces.append(("dub", ds, ds + (ve - vs)))
        cursor = ve
    if vdur > cursor + 0.05:
        pieces.append(("orig", cursor, vdur))
    return pieces


def build_track(hq_audio, dubfile, runs, vdur, out_path, bitrate="160k"):
    """Render one AAC track on the video's timeline: dub inside the runs,
    the release's own audio elsewhere. Residual mismatch at a boundary is
    bounded by FINE_STEP."""
    pieces = pieces_for(runs, vdur)
    filters, labels = [], []
    for i, (src, s, e) in enumerate(pieces):
        inp = 0 if src == "orig" else 1
        filters.append(
            f"[{inp}:a:0]atrim=start={s:.3f}:end={e:.3f},asetpts=PTS-STARTPTS,"
            f"aresample=44100,aformat=sample_fmts=fltp:channel_layouts=stereo[p{i}]")
        labels.append(f"[p{i}]")
    filters.append("".join(labels) + f"concat=n={len(pieces)}:v=0:a=1[out]")
    dubmux.run(["ffmpeg", "-nostdin", "-y", "-v", "error", "-i", hq_audio, "-i", dubfile,
                "-filter_complex", ";".join(filters), "-map", "[out]",
                "-c:a", "aac", "-b:a", bitrate, "-movflags", "+faststart", "-f", "ipod",
                out_path + ".part"])
    os.replace(out_path + ".part", out_path)
    return pieces


def align(video_url, dubfile, workdir, key, bitrate="160k"):
    """Full pipeline; returns the report (with 'aligned' path on accept)."""
    hq = fetch_hq_audio(video_url, os.path.join(workdir, f"{key}.hq.mka"))
    rep = analyse(decode_all(hq), decode_all(dubfile))
    if rep["verdict"] == "accept":
        out = os.path.join(workdir, f"{key}.aligned.m4a")
        rep["pieces"] = [(s, round(a, 2), round(b, 2)) for s, a, b in
                         build_track(hq, dubfile, rep["runs"], rep["video_duration"], out, bitrate)]
        rep["aligned"] = out
    return rep


if __name__ == "__main__":
    import sys
    video, dub, workdir = sys.argv[1], sys.argv[2], sys.argv[3]
    os.makedirs(workdir, exist_ok=True)
    print(json.dumps(align(video, dub, workdir, "cli"), indent=1))
