"""Keep original text tracks during the existing video read; never fetch video."""
import json
import os
import re
import shutil
import subprocess
import time
import uuid
from pathlib import Path

MAX_BYTES = 2_000_000
MAX_FILES = 512
BUDGET_BYTES = 200 * 1024 * 1024
TTL_SECONDS = 30 * 86400
# Lossless codecs supported by the subtitle-only Matroska staging container.
# No subtitle decoder may abort the main video copy.
TEXT_CODECS = {"srt", "subrip", "ass", "ssa"}


def checked_plan(value):
    if not isinstance(value, dict):
        return None
    try:
        identity = str(uuid.UUID(value["identity"]))
        tracks = sorted({t["index"] for t in value["tracks"]
                         if isinstance(t, dict) and type(t.get("index")) is int
                         and 0 <= t["index"] <= 1024
                         and t.get("codec", "").lower() in TEXT_CODECS})
    except (KeyError, ValueError, TypeError, AttributeError):
        return None
    return {"identity": identity, "tracks": tracks[:64]} if tracks else None


def register(match_dir, video):
    plan = checked_plan(video.get("subtitle_source"))
    if plan:
        path = Path(match_dir) / f"subtitle-plan-{video['id']}.json"
        temp = path.with_suffix(".json.tmp")
        temp.write_text(json.dumps(plan))
        os.replace(temp, path)


def outputs(match_dir, video_id, staging):
    """Additional OUTPUTS on the same ffmpeg input, not another invocation."""
    try:
        plan = json.loads((Path(match_dir) / f"subtitle-plan-{video_id}.json").read_text())
        identity = str(uuid.UUID(plan["identity"]))
        tracks = plan["tracks"]
        if not tracks or len(tracks) > 64 or any(type(i) is not int or not 0 <= i <= 1024 for i in tracks):
            return None, []
    except (OSError, ValueError, KeyError, TypeError):
        return None, []
    staging = Path(staging)
    staging.mkdir(parents=True, exist_ok=True)
    args = []
    for index in tracks:
        args += ["-map", f"0:{index}"]
    # Native packet copy during the one video pass; text decoding happens
    # afterwards on this local, subtitle-only file. A bad ASS cannot make
    # a completed video get discarded and downloaded again.
    (staging / "tracks.json").write_text(json.dumps(tracks))
    args += ["-c:s", "copy", "-f", "matroska", "-fs", str(64 * MAX_BYTES + 1),
             str(staging / "embedded.mks")]
    return identity, args


def cached_path(cache_dir, identity, index, now=None):
    """Disk lookup only. An absent or incomplete entry stays absent."""
    try:
        identity = str(uuid.UUID(identity))
        if not 0 <= index <= 1024:
            return None
        path = Path(cache_dir) / identity / f"{index}.srt"
        stat = path.stat()
        if 0 < stat.st_size <= MAX_BYTES and (now or time.time()) - stat.st_mtime <= TTL_SECONDS:
            return path
    except (OSError, ValueError, TypeError):
        pass
    return None


def publish(cache_dir, identity, staging):
    """Called only after the complete raw video passes its existing checks."""
    if not identity:
        return
    target = Path(cache_dir) / str(uuid.UUID(identity))
    target.mkdir(parents=True, exist_ok=True)
    for source in Path(staging).glob("*.srt"):
        if not source.stem.isdecimal() or not 0 < source.stat().st_size <= MAX_BYTES:
            continue
        body = source.read_text(encoding="utf-8", errors="strict")
        # Delivery performs the full shared Remux quality check as well.
        if not re.search(r"\d\d:\d\d:\d\d,\d{3} --> \d\d:\d\d:\d\d,\d{3}\s*\n\S", body):
            continue
        temp = target / (source.name + ".tmp")
        shutil.copyfile(source, temp)
        os.replace(temp, target / source.name)
    sweep(cache_dir)


def finish(cache_dir, identity, staging):
    """Convert only the local captured text; failures cannot invalidate video."""
    staging = Path(staging)
    capture = staging / "embedded.mks"
    if not identity or not capture.is_file() or capture.stat().st_size > 64 * MAX_BYTES:
        return
    tracks = json.loads((staging / "tracks.json").read_text())
    deadline = time.monotonic() + 30
    for ordinal, index in enumerate(tracks):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break
        output = staging / f"{index}.srt"
        try:
            result = subprocess.run(["ffmpeg", "-nostdin", "-y", "-v", "error", "-copyts",
                                     "-i", str(capture), "-map", f"0:s:{ordinal}", "-c:s", "srt",
                                     "-f", "srt", "-fs", str(MAX_BYTES + 1), str(output)],
                                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                    timeout=min(10, remaining))
            if result.returncode:
                output.unlink(missing_ok=True)
        except subprocess.SubprocessError:
            output.unlink(missing_ok=True)
    publish(cache_dir, identity, staging)


def sweep(cache_dir):
    now = time.time()
    files = []
    for path in Path(cache_dir).glob("*/*.srt"):
        try:
            stat = path.stat()
            if stat.st_size > MAX_BYTES or now - stat.st_mtime > TTL_SECONDS:
                path.unlink(missing_ok=True)
            else:
                files.append((stat.st_mtime, stat.st_size, path))
        except FileNotFoundError:
            pass
    total = sum(size for _, size, _ in files)
    count = len(files)
    for _, size, path in sorted(files):
        if total <= BUDGET_BYTES and count <= MAX_FILES:
            break
        path.unlink(missing_ok=True)
        total -= size
        count -= 1
