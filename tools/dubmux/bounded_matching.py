"""Private loopback input for FFmpeg, with bounded upstream reads for matching.

This is not a playback endpoint. Only one registered input is served, under an
unpredictable path, while its separate matching lease is held by the caller.
"""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import os
import re
import socket
import threading
import uuid

from indexed_subtitles import RangeReader, Unavailable
from provider_budget import Limited

MAX_BYTES = int(os.environ.get('DUBMUX_MATCH_MAX_BYTES', str(2 * 1024**3)))
RESERVATION_BYTES = 64 * 1024**2
MAX_REQUESTS = 6000
TIMEOUT = 300
CONNECTIONS = 4
CHUNK = 256 * 1024
# Reject playlists/external protocol inputs: nested media URLs would otherwise
# let the demuxer bypass this reader's byte/accounting boundary.
INPUT_OPTIONS = ('-format_whitelist', 'matroska,mov', '-protocol_whitelist', 'http,tcp',
                 '-probesize', '1048576', '-analyzeduration', '100000', '-fpsprobesize', '0',
                 '-rw_timeout', '15000000')


class MatchingInput:
    def __init__(self, url, size, reserve, resolved, max_bytes=MAX_BYTES,
                 max_requests=MAX_REQUESTS, timeout=TIMEOUT, request_gap=.05):
        self.reader = RangeReader(url, size, resolved=resolved, max_bytes=min(size or max_bytes, max_bytes),
                                  max_requests=max_requests, timeout=timeout,
                                  request_gap=request_gap, reserve=reserve)
        self.path = '/' + uuid.uuid4().hex + '/media'
        self.failure = None
        self.lock = threading.Lock()
        self.closing = threading.Event()
        self.clients = set()
        self.slots = threading.BoundedSemaphore(CONNECTIONS)
        self.server = self.thread = None

    def fail(self, exc):
        reason = exc if isinstance(exc, Limited) else Limited('matching_' + (
            str(exc) if isinstance(exc, Unavailable) else 'read_failed'), 3600)
        with self.lock:
            if self.failure is None:
                self.failure = reason
        self.reader.cancelled.set()

    def check(self):
        if self.failure:
            raise self.failure

    def stats(self):
        return {'bytes': self.reader.bytes_read, 'requests': self.reader.requests,
                'ceiling': self.reader.max_bytes}

    def __enter__(self):
        # Learn/verify length and destination through the same bounded reader;
        # neither urllib nor FFmpeg gets an unmetered resolver/media request.
        try:
            self.reader.read(0, 1, prefetch=False)
        except Exception as exc:
            self.fail(exc)
            self.check()
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def setup(self):
                super().setup()
                self.connection.settimeout(10)
                # A large loopback send buffer can read far ahead after a seek.
                self.connection.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 64 * 1024)

            def log_message(self, *_args):
                pass

            def do_HEAD(self):
                self.serve(False)

            def do_GET(self):
                self.serve(True)

            def serve(self, body):
                if self.path != owner.path:
                    self.send_error(404)
                    return
                if owner.failure or owner.closing.is_set():
                    self.send_error(503)
                    return
                header = self.headers.get('Range')
                match = re.fullmatch(r'bytes=(\d+)-(\d*)', header or '')
                if header and not match:
                    self.send_error(416)
                    return
                total = owner.reader.total
                first = int(match[1]) if match else 0
                last = min(int(match[2]) if match and match[2] else total - 1, total - 1)
                if not 0 <= first <= last < total:
                    self.send_error(416)
                    return
                while not owner.slots.acquire(timeout=.1):
                    if owner.closing.is_set() or owner.failure:
                        self.send_error(503)
                        return
                try:
                    with owner.lock:
                        owner.clients.add(self.connection)
                    if owner.closing.is_set():
                        return
                    self.send_response(206 if match else 200)
                    self.send_header('Content-Length', str(last - first + 1))
                    self.send_header('Accept-Ranges', 'bytes')
                    self.send_header('Content-Type', 'application/octet-stream')
                    if match:
                        self.send_header('Content-Range', f'bytes {first}-{last}/{total}')
                    self.end_headers()
                    if body:
                        offset = first
                        while offset <= last and not owner.closing.is_set():
                            data = owner.reader.read(offset, min(CHUNK, last - offset + 1), prefetch=False)
                            try:
                                self.wfile.write(data)
                            except (BrokenPipeError, ConnectionResetError, socket.timeout):
                                # FFmpeg may abandon a seek's old range without
                                # closing it immediately. Stop that connection;
                                # upstream errors/limits still fail the job.
                                break
                            offset += len(data)
                except (BrokenPipeError, ConnectionResetError):
                    pass  # FFmpeg closes one range when seeking; stop fetching.
                except Exception as exc:
                    if not owner.closing.is_set():
                        owner.fail(exc)
                finally:
                    with owner.lock:
                        owner.clients.discard(self.connection)
                    owner.slots.release()

        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever,
                                       kwargs={'poll_interval': .05}, daemon=True)
        self.thread.start()
        self.url = f'http://127.0.0.1:{self.server.server_port}{self.path}'
        return self

    def close(self):
        self.closing.set()
        self.reader.cancelled.set()
        with self.lock:
            clients = list(self.clients)
        for client in clients:
            try:
                client.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        if self.server:
            self.server.shutdown()
            self.server.server_close()  # Join request threads before releasing the lease.
            self.thread.join()
            self.server = None

    def __exit__(self, *_args):
        self.close()
        self.check()  # Even an FFmpeg "success" after a truncated read is deferred.
