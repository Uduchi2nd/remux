#!/usr/bin/env python3
"""dubmux cache warmer: prepare Vietnamese dubs ahead of time for the trending
Chinese/Korean titles remux already lists (its promoted AIOMetadata libraries),
so the first play of a popular episode finds the dub audio aligned and the
release's segment table recorded (playback then starts in ~1 s).

How: for the latest aired episodes of every series in the warm libraries (and
the movies) this script does what remux's refresh would do, but WITHOUT
touching remux: it asks vnphim for the dub streams and AIOStreams for the HQ
releases (the same candidate rules as remux, mirrored from
services/dubmux.rs), and asks the muxer to prepare the best pair at background
priority (300 — behind playback, next episode, opened items and the walk).
The muxer indexes every finished pair by CONTENT (dub origin playlist x
release file name), so when remux later prepares the same pair under its own
row ids it is answered instantly from the warmed records. Nothing is
pre-muxed: the play cache stays for what is actually watched.

Why not remux's own background queue: its worker handles one job per ~35 s
and re-arms every active series every 12 min, so a lowest-priority job never
ran while anything had been watched in the last 24 h (2026-09-27).

Pacing: one pair at a time, at most WARM_BATCH episodes per run, skipped
while the muxer's general pool has a deep backlog; an episode is not
re-warmed within WARM_REPEAT_DAYS. AIOStreams calls are spaced (it 403s
bursts). Root cron on nimo once a day (03:40 local); `PAUSED` file in this dir skips runs.
"""
import hashlib, json, os, sys, time, urllib.parse, urllib.request
from datetime import datetime, timezone

HERE = os.path.dirname(os.path.abspath(__file__))
STATE = os.path.join(HERE, "state.json")
ENV = dict(l.strip().split("=", 1) for l in open("/root/dubmux-eval/.env") if "=" in l)
BLOB, AIO, MUXER = ENV["VNPHIM_BLOB"], ENV["AIO_BASE"], ENV["MUXER"]
VNPHIM = "http://10.10.10.15:8080"
REMUX = "http://10.10.10.13:3000"
LIBRARIES = [  # remux view names, in priority order
    "Hot Chinese Shows", "Hot Korean Shows", "Top 10 TV Shows on Netflix (South Korea)",
    "Trending Shows", "Hot Korean Movies", "Trending Movies",
]
COUNTRIES = {"China", "Hong Kong", "Taiwan", "South Korea", "Korea"}
MAX_TITLES = int(os.environ.get("WARM_MAX_TITLES", "100"))
EPISODES = int(os.environ.get("WARM_EPISODES", "10"))      # latest aired per series
BATCH = int(os.environ.get("WARM_BATCH", "12"))             # episodes per run
REPEAT_DAYS = int(os.environ.get("WARM_REPEAT_DAYS", "7"))
MUXER_MAX_WAITING = int(os.environ.get("WARM_MUXER_MAX_WAITING", "30"))
PRIORITY = 300
DUB_TOKENS = (".ThuyetMinh.", ".LongTieng.")
MAX_HQ, MAX_DUBS, MAX_PAIRS = 2, 2, 2   # each new pair costs a debrid link
AIO_PACE_S = 3.0


def log(*a):
    print(time.strftime("%Y-%m-%d %H:%M:%S"), *a, flush=True)


def get(url, timeout=60, headers=None):
    req = urllib.request.Request(url, headers=headers or {"User-Agent": "dubmux-warm/1"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read())


def remux(path):
    kv = dict(l.strip().split("=", 1) for l in open("/root/.remux-admin") if "=" in l)
    return get(REMUX + path, 60, {"Authorization": f'MediaBrowser Token="{kv["token"]}"',
                                  "User-Agent": "dubmux-warm/1"})


def aired(item, now):
    p = item.get("PremiereDate")
    try:
        return bool(p) and datetime.fromisoformat(p.replace("Z", "+00:00")) <= now
    except ValueError:
        return False


def candidates():
    """[{key, type, tmdb, season, episode, label}] in warm order."""
    now = datetime.now(timezone.utc)
    user = remux("/Users/Me")["Id"]
    views = {v["Name"]: v["Id"] for v in remux(f"/Users/{user}/Views")["Items"]}
    titles, seen = [], set()
    for name in LIBRARIES:
        vid = views.get(name)
        if not vid:
            log(f"library {name!r} not found in remux views")
            continue
        for it in remux(f"/Users/{user}/Items?ParentId={vid}&Fields=ProviderIds,PremiereDate,ProductionLocations&Limit=100")["Items"]:
            tmdb = (it.get("ProviderIds") or {}).get("Tmdb")
            if it["Id"] in seen or it["Type"] not in ("Series", "Movie") or not tmdb:
                continue
            if not (set(it.get("ProductionLocations") or []) & COUNTRIES):
                continue
            seen.add(it["Id"])
            titles.append((name, it, tmdb))
    out = []
    for lib, it, tmdb in titles[:MAX_TITLES]:
        if it["Type"] == "Movie":
            if aired(it, now):
                out.append({"key": it["Id"], "type": "movie", "tmdb": tmdb, "label": f"{it['Name']} ({lib})",
                            "aired": it.get("PremiereDate") or ""})
            continue
        try:
            eps = remux(f"/Shows/{it['Id']}/Episodes?UserId={user}&Fields=PremiereDate")["Items"]
        except Exception as e:  # noqa: BLE001
            log(f"episodes of {it['Name']!r} failed: {e}")
            continue
        eps = [e for e in eps if aired(e, now) and (e.get("ParentIndexNumber") or 0) > 0]
        eps.sort(key=lambda e: (e.get("ParentIndexNumber") or 0, e.get("IndexNumber") or 0))
        for e in eps[-EPISODES:]:
            out.append({"key": e["Id"], "type": "series", "tmdb": tmdb, "season": e.get("ParentIndexNumber"),
                        "episode": e.get("IndexNumber"), "label": f"{it['Name']} S{e.get('ParentIndexNumber')}E{e.get('IndexNumber')} ({lib})",
                        "aired": e.get("PremiereDate") or ""})
    # With a small daily batch the NEWEST aired episodes across all titles go
    # first (what people open next), older back-catalogue fills later days.
    out.sort(key=lambda c: c["aired"], reverse=True)
    return out


# --- candidate rules, mirrored from remux services/dubmux.rs (as in the eval harness)
def fname(s):
    return (s.get("behaviorHints") or {}).get("filename") or ""


def is_hq(s):
    f = fname(s).lower(); u = s.get("url") or ""
    if not (u.startswith("http") and "vnphim" not in u):
        return False
    if any(t.lower() in f for t in DUB_TOKENS):
        return False
    if any(t in f for t in (".kkphim.", ".ophim.", ".hotphim.", ".yanhh3d.", "proxiedvn", ".vietsub.")):
        return False
    ext = f.rsplit(".", 1)[-1] if "." in f else ""
    return f.endswith(".mkv") or f.endswith(".mp4") or ext not in ("m3u8", "m3u", "ts", "strm", "avi", "wmv", "flv", "iso", "rar")


def is_dub(s):
    return any(t in fname(s) for t in DUB_TOKENS) and (s.get("url") or "").startswith("http")


def hid(prefix, s):
    return prefix + hashlib.sha1((fname(s) + "|" + (s.get("url") or "")).encode()).hexdigest()[:24]


def sid(it):
    return f"tmdb:{it['tmdb']}" + (f":{it['season']}:{it['episode']}" if it["type"] == "series" else "")


def prepare(dub, hq, key):
    body = json.dumps({"dub": {"id": key[0], "url": dub["url"]}, "video": {"id": key[1], "url": hq["url"]},
                       "wait": 0, "priority": PRIORITY}).encode()
    req = urllib.request.Request(f"{MUXER}/prepare", data=body, headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=60) as r:
        return json.loads(r.read())


def wait_done(key, budget=2400):
    t0 = time.time()
    while time.time() - t0 < budget:
        st = get(f"{MUXER}/status/{key[0]}/{key[1]}", 30)
        if st.get("status") != "running":
            return st
        time.sleep(15)
    return {"status": "timeout"}


def muxer_backlog():
    try:
        q = get(f"{MUXER}/queue", 20)
        return sum(1 for g in ("extract", "match") for prio, _ in q[g]["waiting"] if prio <= PRIORITY)
    except Exception as e:  # noqa: BLE001
        log(f"muxer queue unavailable: {e}")
        return None


def warm(it):
    """Prepare the best dub x HQ pair of one episode/movie. Returns outcome."""
    vs = get(f"{VNPHIM}/{BLOB}/stream/{it['type']}/{sid(it)}.json", 180).get("streams", [])
    dubs = [s for s in vs if is_dub(s)][:MAX_DUBS]
    if not dubs:
        return "no_dub"
    sys.path.insert(0, "/root/dubmux-eval")
    import aiopace; aiopace.wait_turn()
    aio = get(f"{AIO}/stream/{it['type']}/{sid(it)}.json", 120).get("streams", [])
    hqs = [s for s in aio if is_hq(s)][:MAX_HQ]
    if not hqs:
        return "no_hq"
    pairs = [(dub, hq) for hq in hqs for dub in dubs][:MAX_PAIRS]
    for dub, hq in pairs:
        key = (hid("wmd", dub), hid("wmh", hq))
        try:
            r = prepare(dub, hq, key)
            st = r if r.get("status") != "running" else wait_done(key)
        except Exception as e:  # noqa: BLE001
            log(f"  prepare error: {str(e)[:120]}")
            continue
        if st.get("status") == "ready":
            res = st.get("result") or {}
            return "accepted" + (" (reused)" if res.get("aliased_from") or st.get("cached") else "")
    return "no_match"


PAUSE_FLAG = os.path.join(HERE, "PAUSED")


def main():
    if os.path.exists(PAUSE_FLAG):
        log(f"paused ({PAUSE_FLAG} exists); skipping this run")
        return
    state = {}
    if os.path.exists(STATE):
        try:
            state = json.load(open(STATE))
        except Exception:  # noqa: BLE001
            state = {}
    try:
        brk = get(f"{MUXER}/health", 20).get("breaker_secs", 0)
    except Exception:  # noqa: BLE001
        brk = 0
    if brk:
        log(f"muxer resolver breaker open ({brk}s left: debrid rate limit); skipping this run")
        return
    b = muxer_backlog()
    if b is None or b > MUXER_MAX_WAITING:
        log(f"muxer has {b} pairs waiting at or ahead of warm priority (> {MUXER_MAX_WAITING}); skipping this run")
        return
    cands = candidates()
    cutoff = time.time() - REPEAT_DAYS * 86400
    fresh = [c for c in cands if state.get(c["key"], 0) < cutoff]
    batch = fresh[:BATCH]
    log(f"{len(cands)} candidate episodes/movies, {len(fresh)} not warmed in {REPEAT_DAYS} d, warming {len(batch)} (muxer backlog {b})")
    for it in batch:
        t0 = time.time()
        try:
            outcome = warm(it)
        except Exception as e:  # noqa: BLE001
            outcome = f"error: {str(e)[:100]}"
        log(f"  {outcome:22} {it['label']}  {time.time() - t0:.0f}s")
        if not outcome.startswith("error"):
            state[it["key"]] = time.time()
            state = {k: v for k, v in state.items() if v > time.time() - 3 * REPEAT_DAYS * 86400}
            json.dump(state, open(STATE + ".tmp", "w"))
            os.replace(STATE + ".tmp", STATE)


if __name__ == "__main__":
    try:
        main()
    except Exception as e:  # noqa: BLE001
        log(f"FAILED: {e}")
        sys.exit(1)
