#!/usr/bin/env python3
"""dubmux cache warmer: prepare Vietnamese dubs ahead of time for the trending
Chinese/Korean titles remux already lists (its promoted AIOMetadata libraries),
so the first play of a popular episode finds the dub audio aligned and the
release's segment table recorded (playback then starts in ~1 s).

How: the latest aired episodes of every series in the warm libraries (and the
movies) are put on remux's persistent background stream-refresh queue
(`background_stream_refresh_jobs`, the same queue playback uses for the
next/recent episodes) at a priority BELOW every viewer-driven job. The queue
worker refreshes the episode's streams, which creates the "[+VN dub]" rows and
asks the muxer to prepare each pair at its background priority (300, behind
playback and the on-play walk). Nothing is pre-muxed: the play cache stays for
what is actually watched; the durable audio cache (200 GB / 1 y) holds the
aligned dubs.

Pacing: at most WARM_BATCH episodes per run, only while the muxer's queue and
remux's queue are shallow; an episode is not re-queued within WARM_REPEAT_DAYS.
Runs from root cron on nimo every 2 h; writes to the remux DB through a
throwaway python container in LXC 111 (short WAL transactions beside the
running server).
"""
import json, os, subprocess, sys, time, urllib.request
from datetime import datetime, timezone, timedelta

HERE = os.path.dirname(os.path.abspath(__file__))
STATE = os.path.join(HERE, "state.json")
REMUX = "http://10.10.10.13:3000"
MUXER_QUEUE = "http://100.68.133.6:12500/queue"
LIBRARIES = [  # remux view names, in priority order
    "Hot Chinese Shows", "Hot Korean Shows", "Top 10 TV Shows on Netflix (South Korea)",
    "Trending Shows", "Hot Korean Movies", "Trending Movies",
]
COUNTRIES = {"China", "Hong Kong", "Taiwan", "South Korea", "Korea"}
MAX_TITLES = int(os.environ.get("WARM_MAX_TITLES", "100"))
EPISODES = int(os.environ.get("WARM_EPISODES", "10"))      # latest aired per series
BATCH = int(os.environ.get("WARM_BATCH", "40"))             # episodes queued per run
REPEAT_DAYS = int(os.environ.get("WARM_REPEAT_DAYS", "7"))
MUXER_MAX_WAITING = int(os.environ.get("WARM_MUXER_MAX_WAITING", "80"))
REMUX_MAX_PENDING = int(os.environ.get("WARM_REMUX_MAX_PENDING", "20"))
PRIORITY = 10   # remux queue: higher runs first; playback scopes use 40/100/200


def log(*a):
    print(time.strftime("%Y-%m-%d %H:%M:%S"), *a, flush=True)


def creds():
    kv = dict(l.strip().split("=", 1) for l in open("/root/.remux-admin") if "=" in l)
    return kv["token"]


def api(path, token):
    req = urllib.request.Request(REMUX + path, headers={
        "Authorization": f'MediaBrowser Token="{token}"', "User-Agent": "dubmux-warm/1"})
    with urllib.request.urlopen(req, timeout=60) as r:
        return json.loads(r.read())


def aired(item, now):
    p = item.get("PremiereDate")
    if not p:
        return False
    try:
        return datetime.fromisoformat(p.replace("Z", "+00:00")) <= now
    except ValueError:
        return False


def candidates(token):
    """[(media_id, series_id|None, label)] in warm order."""
    now = datetime.now(timezone.utc)
    user = api("/Users/Me", token)["Id"]
    views = {v["Name"]: v["Id"] for v in api(f"/Users/{user}/Views", token)["Items"]}
    titles, seen = [], set()
    for name in LIBRARIES:
        vid = views.get(name)
        if not vid:
            log(f"library {name!r} not found in remux views")
            continue
        items = api(f"/Users/{user}/Items?ParentId={vid}&Fields=ProviderIds,PremiereDate,ProductionLocations&Limit=100", token)["Items"]
        for it in items:
            if it["Id"] in seen or it["Type"] not in ("Series", "Movie"):
                continue
            if not (set(it.get("ProductionLocations") or []) & COUNTRIES):
                continue
            seen.add(it["Id"])
            titles.append((name, it))
    titles = titles[:MAX_TITLES]
    out = []
    for lib, it in titles:
        if it["Type"] == "Movie":
            if aired(it, now):
                out.append((it["Id"], None, f"{it['Name']} ({lib})"))
            continue
        try:
            eps = api(f"/Shows/{it['Id']}/Episodes?UserId={user}&Fields=PremiereDate", token)["Items"]
        except Exception as e:  # noqa: BLE001
            log(f"episodes of {it['Name']!r} failed: {e}")
            continue
        eps = [e for e in eps if aired(e, now) and e.get("ParentIndexNumber", 1) > 0]
        eps.sort(key=lambda e: (e.get("ParentIndexNumber") or 0, e.get("IndexNumber") or 0))
        for e in eps[-EPISODES:]:
            out.append((e["Id"], it["Id"], f"{it['Name']} S{e.get('ParentIndexNumber')}E{e.get('IndexNumber')} ({lib})"))
    return out


def muxer_waiting():
    try:
        with urllib.request.urlopen(MUXER_QUEUE, timeout=20) as r:
            q = json.loads(r.read())
        # only work that runs at or ahead of the warm prepares (muxer priority
        # 300) matters; evaluation runs queue at 400 and would starve nothing
        return sum(1 for g in ("extract", "match") for prio, _ in q[g]["waiting"] if prio <= 300)
    except Exception as e:  # noqa: BLE001
        log(f"muxer queue unavailable: {e}")
        return None


def remux_db(script, payload=None):
    """Run a python snippet against remux's SQLite inside LXC 111."""
    cmd = ["pct", "exec", "111", "--", "docker", "run", "--rm", "-i", "-v", "/opt/remux/data:/data",
           "python:3.12-slim", "python3", "-c", script]
    r = subprocess.run(cmd, input=json.dumps(payload or {}), capture_output=True, text=True, timeout=300)
    if r.returncode != 0:
        raise RuntimeError(r.stderr.strip()[-400:])
    return json.loads(r.stdout.strip().splitlines()[-1])


PENDING_SQL = """
import sqlite3, json, sys
c = sqlite3.connect("/data/db.sqlite", timeout=30)
n = c.execute("select count(*) from background_stream_refresh_jobs where priority = ?", (%d,)).fetchone()[0]
print(json.dumps({"pending": n}))
""" % PRIORITY

INSERT_SQL = """
import sqlite3, json, sys
from datetime import datetime, timezone
jobs = json.load(sys.stdin)["jobs"]
c = sqlite3.connect("/data/db.sqlite", timeout=30)
now = datetime.now(timezone.utc).strftime("%%Y-%%m-%%d %%H:%%M:%%S.%%f") + "000"
ins = 0
with c:
    for j in jobs:
        cur = c.execute(
            "INSERT INTO background_stream_refresh_jobs (user_id, media_id, series_id, priority, run_after, created_at, updated_at) "
            "VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT(user_id, media_id) DO NOTHING",
            (bytes.fromhex(j["user"]), bytes.fromhex(j["media"]), bytes.fromhex(j["series"]) if j["series"] else None, %d, now, now, now))
        ins += cur.rowcount
print(json.dumps({"inserted": ins}))
""" % PRIORITY


def main():
    token = creds()
    user = api("/Users/Me", token)["Id"].replace("-", "")
    state = {}
    if os.path.exists(STATE):
        try:
            state = json.load(open(STATE))
        except Exception:  # noqa: BLE001
            state = {}
    w = muxer_waiting()
    if w is None or w > MUXER_MAX_WAITING:
        log(f"muxer queue has {w} waiting (> {MUXER_MAX_WAITING}); skipping this run")
        return
    pending = remux_db(PENDING_SQL)["pending"]
    if pending > REMUX_MAX_PENDING:
        log(f"remux queue still has {pending} warm jobs pending; skipping this run")
        return
    cands = candidates(token)
    cutoff = time.time() - REPEAT_DAYS * 86400
    fresh = [c for c in cands if state.get(c[0], 0) < cutoff]
    batch = fresh[:BATCH]
    log(f"{len(cands)} candidate episodes/movies, {len(fresh)} not warmed in {REPEAT_DAYS} d, queuing {len(batch)} "
        f"(muxer waiting {w}, remux pending {pending})")
    if not batch:
        return
    res = remux_db(INSERT_SQL, {"jobs": [{"user": user, "media": m, "series": s} for m, s, _ in batch]})
    now = time.time()
    for m, _, label in batch:
        state[m] = now
        log("  queued", label)
    # keep the state small: forget entries older than 3 x the repeat window
    state = {k: v for k, v in state.items() if v > now - 3 * REPEAT_DAYS * 86400}
    json.dump(state, open(STATE + ".tmp", "w"))
    os.replace(STATE + ".tmp", STATE)
    log(f"inserted {res['inserted']} jobs")


if __name__ == "__main__":
    try:
        main()
    except Exception as e:  # noqa: BLE001
        log(f"FAILED: {e}")
        sys.exit(1)
