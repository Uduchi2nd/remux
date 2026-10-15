"""Shared admission limits for server-side media reads, not device playback.

The byte ledger records conservative reservations, not measured traffic.
Failed reads keep their reservation; restarting never refills a daily budget.
"""
from contextlib import closing, contextmanager
from dataclasses import dataclass
import hashlib
import json
import os
import re
from pathlib import Path
import sqlite3
import threading
import time
from urllib.parse import urlsplit
import uuid


@dataclass(frozen=True)
class Policy:
    daily_bytes: int
    concurrency: int
    full_gap: float
    probe_gap: float


class Limited(RuntimeError):
    def __init__(self, reason, retry_after=60):
        self.reason = reason
        self.retry_after = max(1, int(retry_after))
        super().__init__("provider media work deferred: " + reason)


def policies_from_env():
    def policy(prefix, daily, concurrency, gap, probe):
        return Policy(int(float(os.environ.get(prefix + "_DAILY_GB", daily)) * 1_000_000_000),
                      int(os.environ.get(prefix + "_CONCURRENCY", concurrency)),
                      float(os.environ.get(prefix + "_START_GAP_S", gap)),
                      float(os.environ.get(prefix + "_PROBE_GAP_S", probe)))
    return {"premiumize": policy("DUBMUX_PREMIUMIZE", "10", "1", "3600", "60"),
            "torbox": policy("DUBMUX_TORBOX", "80", "3", "10", "5")}


def provider(value):
    value = str(value or "").lower().replace("-", "").replace("_", "")
    if value in ("torbox", "tb"):
        return "torbox"
    if value == "other":
        return "other"
    # Unknown paid sources share the strict Premiumize bucket.
    return "premiumize"


class Ledger:
    def __init__(self, path, policies=None):
        self.path = Path(path)
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.policies = policies or policies_from_env()
        with closing(self.connect()) as c:
            c.executescript("""
                CREATE TABLE IF NOT EXISTS usage(day INTEGER, provider TEXT, reserved INTEGER NOT NULL,
                  PRIMARY KEY(day, provider));
                CREATE TABLE IF NOT EXISTS leases(id TEXT PRIMARY KEY, provider TEXT, expires REAL);
                CREATE TABLE IF NOT EXISTS starts(provider TEXT, kind TEXT, next REAL,
                  PRIMARY KEY(provider, kind));
            """)
        self.path.chmod(0o600)

    def connect(self):
        return sqlite3.connect(self.path, timeout=10, isolation_level=None)

    def reserve(self, service, size, kind="full", now=None, check_only=False):
        service = provider(service)
        if service == "other":
            return None
        if kind not in ("full", "probe", "indexed"):
            raise ValueError("invalid read kind")
        if type(size) is not int or size <= 0:
            raise Limited("unknown_file_size", 3600)
        # Probe allowance is an estimate. Full jobs reserve the whole release
        # plus 10% for metadata/seeks. This is not a hard network byte meter.
        cost = (min(size, 128 * 1024 * 1024) if kind == "probe" else
                size if kind == "indexed" else (size * 11 + 9) // 10)
        clock = "probe" if kind == "probe" else "full"
        now = time.time() if now is None else now
        day = int(now // 86400)
        policy = self.policies[service]
        c = self.connect()
        try:
            c.execute("BEGIN IMMEDIATE")
            c.execute("DELETE FROM leases WHERE expires<=?", (now,))
            c.execute("DELETE FROM usage WHERE day<?", (day - 45,))
            used = c.execute("SELECT reserved FROM usage WHERE day=? AND provider=?", (day, service)).fetchone()
            used = used[0] if used else 0
            active = c.execute("SELECT COUNT(*) FROM leases WHERE provider=?", (service,)).fetchone()[0]
            next_start = c.execute("SELECT next FROM starts WHERE provider=? AND kind=?", (service, clock)).fetchone()
            if cost > policy.daily_bytes - used:
                raise Limited("daily_reservation_budget", (day + 1) * 86400 - now)
            if active >= policy.concurrency:
                raise Limited("concurrency", 30)
            if next_start and next_start[0] > now:
                raise Limited("start_rate", next_start[0] - now)
            if check_only:
                c.rollback()
                return None
            token = str(uuid.uuid4())
            c.execute("INSERT INTO usage VALUES(?,?,?) ON CONFLICT(day,provider) DO UPDATE SET reserved=reserved+excluded.reserved",
                      (day, service, cost))
            c.execute("INSERT INTO leases VALUES(?,?,?)", (token, service, now + 2 * 3600))
            gap = policy.probe_gap if kind == "probe" else policy.full_gap
            c.execute("INSERT INTO starts VALUES(?,?,?) ON CONFLICT(provider,kind) DO UPDATE SET next=excluded.next",
                      (service, clock, now + gap))
            c.commit()
            return token
        except Exception:
            c.rollback()
            raise
        finally:
            c.close()

    def release(self, token):
        if token:
            with closing(self.connect()) as c:
                c.execute("DELETE FROM leases WHERE id=?", (token,))

    def status(self, now=None):
        now = time.time() if now is None else now
        day = int(now // 86400)
        out = {"accounting": "reserved_estimated_bytes", "day_utc": day, "providers": {}}
        with closing(self.connect()) as c:
            for service, policy in self.policies.items():
                used = c.execute("SELECT reserved FROM usage WHERE day=? AND provider=?", (day, service)).fetchone()
                active = c.execute("SELECT COUNT(*) FROM leases WHERE provider=? AND expires>?", (service, now)).fetchone()[0]
                out["providers"][service] = {"daily_bytes": policy.daily_bytes, "reserved_bytes": used[0] if used else 0,
                                              "active": active, "concurrency": policy.concurrency,
                                              "full_start_gap_seconds": policy.full_gap, "probe_gap_seconds": policy.probe_gap}
        return out


class Lease:
    def __init__(self, close):
        self._close = close
        self._lock = threading.Lock()

    def close(self):
        with self._lock:
            close, self._close = self._close, None
        if close:
            close()


class MediaReads:
    def __init__(self, data):
        self.data = Path(data)
        self.ledger = Ledger(self.data / "provider-budget.sqlite")
        self.directory = self.data / "provider-sources"
        self.directory.mkdir(exist_ok=True)
        self.lock = threading.Lock()
        self.registry_lock = threading.RLock()
        self.active = {}

    def source_path(self, video_id):
        if not re.fullmatch(r"[A-Za-z0-9._-]{1,120}", str(video_id)):
            raise Limited("unregistered_source", 3600)
        return self.directory / (str(video_id) + ".json")

    def register(self, video):
        with self.registry_lock:
            # Called only on an authenticated Remux preparation request.
            path = self.source_path(video["id"])
            old = json.loads(path.read_text()) if path.exists() else {}
            size = video.get("size")
            if type(size) is not int or size <= 0:
                size = 0
            hashes = list(old.get("url_hashes", []))
            digest = hashlib.sha256(video["url"].encode()).hexdigest()
            if digest not in hashes:
                hashes.append(digest)
            declared = provider(video.get("provider"))
            effective = "premiumize" if old.get("detected_premiumize") else declared
            value = {"provider": effective, "size": size, "url_hashes": hashes[-8:],
                     "detected_premiumize": bool(old.get("detected_premiumize"))}
            temp = path.with_suffix(".tmp")
            temp.write_text(json.dumps(value))
            os.replace(temp, path)

    def source(self, video_id, url):
        try:
            path = self.source_path(video_id)
            source = json.loads(path.read_text())
            if hashlib.sha256(url.encode()).hexdigest() not in source["url_hashes"]:
                raise Limited("unregistered_source", 3600)
            return source
        except (OSError, ValueError, KeyError):
            raise Limited("unregistered_source", 3600) from None

    def check_resolved(self, video_id, original, resolved):
        with self.registry_lock:
            source = self.source(video_id, original)
            host = (urlsplit(resolved).hostname or "").lower()
            path = self.source_path(video_id)
            strict = source["provider"] == "torbox" and not (host == "torbox.app" or host.endswith(".torbox.app"))
            # A TorBox label may fail over to another provider. Never allow that
            # unknown CDN to inherit TorBox's larger allowance.
            if strict:
                source.update(provider="premiumize", detected_premiumize=True)
            digest = hashlib.sha256(resolved.encode()).hexdigest()
            if digest not in source["url_hashes"]:
                source["url_hashes"] = (source["url_hashes"] + [digest])[-8:]
            temp = path.with_suffix(".tmp")
            temp.write_text(json.dumps(source))
            os.replace(temp, path)
            if strict:
                raise Limited("provider_changed_retry_strict_budget", 60)

    def acquire(self, video_id, url, reuse=True):
        source = self.source(video_id, url)
        key = (video_id, source["provider"])
        with self.lock:
            if reuse and key in self.active:
                self.active[key][1] += 1
            else:
                token = self.ledger.reserve(source["provider"], source["size"])
                if not reuse:
                    return Lease(lambda: self.ledger.release(token))
                self.active[key] = [token, 1]
        def close():
            with self.lock:
                self.active[key][1] -= 1
                if not self.active[key][1]:
                    token, _ = self.active.pop(key)
                    self.ledger.release(token)
        return Lease(close)

    def check(self, video_id, url):
        source = self.source(video_id, url)
        with self.lock:
            if (video_id, source["provider"]) not in self.active:
                self.ledger.reserve(source["provider"], source["size"], check_only=True)

    @contextmanager
    def scope(self, video_id, url):
        lease = self.acquire(video_id, url)
        try:
            yield
        finally:
            lease.close()
