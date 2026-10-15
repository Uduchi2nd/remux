"""Run in the candidate image with --network=none; fixtures and loopback only."""
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
import uuid


with tempfile.TemporaryDirectory() as directory:
    os.environ.update(DUBMUX_DATA=directory, DUBMUX_TRANSFER_TOKEN="offline-test-token",
                      DUBMUX_PREMIUMIZE_DAILY_GB="0.00000022",
                      DUBMUX_PREMIUMIZE_START_GAP_S="0",
                      DUBMUX_TORBOX_DAILY_GB="0.0000022",
                      DUBMUX_TORBOX_START_GAP_S="0")
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    dub_id, video_id = uuid.uuid4().hex, uuid.uuid4().hex
    key = dub_id + "__" + video_id
    match = Path(directory) / "match"
    match.mkdir()
    (match / (key + ".json")).write_text(json.dumps({
        "verdict": "accept", "lag": 0, "video_url": "https://offline.invalid/video"}))
    identity = str(uuid.uuid4())
    subtitle = Path(directory) / "subtitles" / identity / "22.srt"
    subtitle.parent.mkdir(parents=True)
    body = b"1\n00:00:01,000 --> 00:00:02,000\nOffline cached text\n"
    subtitle.write_bytes(body)

    def start():
        process = subprocess.Popen(["uvicorn", "server:app", "--host", "127.0.0.1", "--port", str(port),
                                    "--log-level", "error"], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        for _ in range(300):
            try:
                if request("/health")[0] == 200:
                    return process
            except OSError:
                if process.poll() is not None:
                    raise RuntimeError("candidate exited before health check")
            time.sleep(0.1)
        process.terminate()
        process.communicate(timeout=10)
        raise RuntimeError("candidate did not start")

    def stop(process):
        process.terminate()
        process.communicate(timeout=10)

    def request(route, payload=None, auth=False):
        headers = {"Authorization": "Bearer offline-test-token"} if auth else {}
        if payload is not None:
            headers["Content-Type"] = "application/json"
        req = urllib.request.Request(f"http://127.0.0.1:{port}" + route,
                                     data=json.dumps(payload).encode() if payload is not None else None,
                                     headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=3) as response:
                return response.status, response.read(), response.headers
        except urllib.error.HTTPError as error:
            return error.code, error.read(), error.headers

    def value(route, payload=None, auth=True):
        status, data, _ = request(route, payload, auth)
        assert status == 200, (route, status)
        return json.loads(data)

    process = start()
    try:
        assert request("/transfer-budget/status")[0] == 401
        assert request("/transfer-budget/reserve", {"bytes": 100})[0] == 401
        assert request("/transfer-budget/reserve", [], True)[0] == 400
        assert request("/transfer-budget/reserve", {"bytes": 0}, True)[0] == 429
        assert request("/transfer-budget/reserve", {"bytes": 100, "kind": "invalid"}, True)[0] == 400

        from test_indexed_subtitles import fixture, source
        media=fixture()
        video={"id":uuid.uuid4().hex,"provider":"premiumize","size":len(media)}
        with source(media) as (url,upstream):
            video["url"]=url
            payload={"video":video,"subtitle_ordinals":[1]}
            assert request("/subtitles/indexed",payload)[0]==401
            assert request("/subtitles/indexed",{"video":video,"subtitle_ordinals":[-1]},True)[0]==400
            assert request("/subtitles/indexed",payload,True)[0]==429
            assert upstream.requests==0, "budget rejection must precede source access"
            video["provider"]="other"
            status,artifact,headers=request("/subtitles/indexed",payload,True)
            assert status==200 and artifact.startswith(b'\x1a\x45\xdf\xa3')
            assert int(headers["X-Remux-Extraction-Bytes"])<len(media)//20
        with source(media,"ignore") as (url,_):
            video["url"]=url
            assert request("/subtitles/indexed",payload,True)[0]==422
        registered=list((Path(directory)/"provider-sources").glob("*.json"))
        assert registered and all(p.name.startswith("index-") for p in registered), "text extraction authorized a full mux source"

        pm = value("/transfer-budget/reserve", {"provider": "premiumize", "bytes": 100})["lease"]
        assert pm == str(uuid.UUID(pm))
        status, data, headers = request("/transfer-budget/reserve", {"provider": "premiumize", "bytes": 100}, True)
        assert status == 429 and json.loads(data)["reason"] == "concurrency"
        assert int(headers["Retry-After"]) > 0
        tb = value("/transfer-budget/reserve", {"provider": "torbox", "bytes": 100})["lease"]
        assert value("/transfer-budget/status")["providers"]["torbox"]["active"] == 1
        value("/transfer-budget/release/" + pm, {})
        value("/transfer-budget/release/" + tb, {})
        pm = value("/transfer-budget/reserve", {"provider": "premiumize", "bytes": 100})["lease"]
        value("/transfer-budget/release/" + pm, {})
        status, data, _ = request("/transfer-budget/reserve", {"provider": "premiumize", "bytes": 1}, True)
        assert status == 429 and json.loads(data)["reason"] == "daily_reservation_budget"

        prepare = {"dub": {"id": dub_id, "url": "https://offline.invalid/dub"},
                   "video": {"id": video_id, "url": "https://offline.invalid/video"}}
        assert value("/prepare", prepare, auth=False)["cached"] is True
        assert request(f"/subtitles/{identity}/22.srt")[:2] == (200, body)
        assert request(f"/mux/{dub_id}/{video_id}/start", {})[0] == 429
        session = Path(directory) / "hls" / key
        session.mkdir()
        (session / ".done").write_text(str(int(time.time())))
        assert value(f"/mux/{dub_id}/{video_id}/start", {})["done"] is True
        prepare["video"]["id"] = uuid.uuid4().hex
        assert request("/prepare", prepare)[0] == 401
        assert value("/jobs") == {}
        before = value("/transfer-budget/status")["providers"]
        assert before["premiumize"]["reserved_bytes"] == 220
        assert before["premiumize"]["active"] == 0
        stop(process)
        process = start()
        assert value("/transfer-budget/status")["providers"] == before
        assert request("/transfer-budget/reserve", {"provider": "premiumize", "bytes": 1}, True)[0] == 429
        assert value("/jobs") == {}
        print(json.dumps({"http_checks": 28, "persistent_budget": True,
                          "media_jobs_started": 0, "external_network": "disabled"}))
    finally:
        stop(process)
