"""Loopback-only byte limits and worker admission-order regressions."""
from concurrent.futures import ThreadPoolExecutor
from contextlib import ExitStack
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import patch
from urllib import request
import http.client
import json
import os
import re
import threading
import time
import unittest

from bounded_matching import MatchingInput
from provider_budget import Limited, MediaReads, Policy, Ledger


class MatchingTest(unittest.TestCase):
    def setUp(self):
        self.temp=TemporaryDirectory();self.addCleanup(self.temp.cleanup)
        self.root=Path(self.temp.name)
        self.data=bytes(range(256))*8
        self.received=0;self.mode='ranges';self.etag='"original"'
        owner=self
        class Origin(BaseHTTPRequestHandler):
            def log_message(self,*args):pass
            def do_GET(self):
                m=re.fullmatch(r'bytes=(\d+)-(\d+)',self.headers.get('Range',''))
                if not m:
                    self.send_error(400);return
                first,last=map(int,m.groups())
                last=min(last,len(owner.data)-1)
                self.send_response(200 if owner.mode=='ignore-range' else 206)
                self.send_header('Content-Range',f'bytes {first}-{last}/{len(owner.data)}')
                self.send_header('Content-Length',str(last-first+1))
                self.send_header('ETag',owner.etag);self.end_headers()
                try:
                    self.wfile.write(owner.data[first:last+1])
                    owner.received+=last-first+1
                except (BrokenPipeError,ConnectionResetError):pass
        self.origin=ThreadingHTTPServer(('127.0.0.1',0),Origin)
        self.thread=threading.Thread(target=self.origin.serve_forever,daemon=True)
        self.thread.start()
        self.addCleanup(self.origin.server_close)
        self.addCleanup(self.origin.shutdown)
        self.url=f'http://127.0.0.1:{self.origin.server_port}/hq.mkv'
        self.video={'id':'hq','url':self.url,'provider':'premiumize','size':len(self.data)}
        self.reads=MediaReads(self.root/'budget')
        self.reads.ledger.policies={p:Policy(10000,3,0,0) for p in ['premiumize','torbox']}
        self.reads.register(self.video)

    def input(self,ceiling=256):
        stack=ExitStack()
        self.addCleanup(stack.close)
        reserve=stack.enter_context(self.reads.matching('hq',self.url,ceiling,16))
        bounded=MatchingInput(self.url,len(self.data),reserve,
                             lambda u:self.reads.check_resolved('hq',self.url,u),
                             max_bytes=ceiling,request_gap=0)
        return stack,bounded

    def fetch(self,url,first,last):
        with request.urlopen(request.Request(url,headers={'Range':f'bytes={first}-{last}'}),timeout=5) as r:
            return r.read()

    def test_seeked_reads_keep_original_bytes_and_do_not_reserve_full_video(self):
        stack,source=self.input()
        with source:
            self.assertEqual(self.fetch(source.url,1000,1031),self.data[1000:1032])
            self.assertEqual(self.fetch(source.url,1000,1031),self.data[1000:1032])
            self.assertEqual(source.stats()['bytes'],33)
        stack.close()
        status=self.reads.ledger.status()['providers']['premiumize']
        self.assertEqual(status['reserved_bytes'],48)
        self.assertEqual(status['active'],0)

    def test_byte_ceiling_defers_even_when_client_ignores_truncation(self):
        stack,source=self.input(ceiling=64)
        with self.assertRaisesRegex(Limited,'matching_byte_limit'):
            with source:
                self.fetch(source.url,0,31)
                try:self.fetch(source.url,32,95)
                except http.client.IncompleteRead:pass
        self.assertLessEqual(source.stats()['bytes'],64)
        self.assertLessEqual(self.received,64)
        stack.close()

    def test_daily_topup_is_denied_before_more_upstream_body_reads(self):
        self.reads.ledger.policies['premiumize']=Policy(48,1,0,0)
        stack,source=self.input()
        with self.assertRaisesRegex(Limited,'daily_reservation_budget'):
            with source:
                self.fetch(source.url,100,131)
                try:self.fetch(source.url,200,231)
                except http.client.IncompleteRead:pass
        self.assertEqual(source.stats()['bytes'],33)
        self.assertEqual(self.reads.ledger.status()['providers']['premiumize']['reserved_bytes'],48)
        stack.close()

    def test_ignored_range_and_changed_source_never_become_matching_success(self):
        self.mode='ignore-range'
        stack,source=self.input()
        with self.assertRaisesRegex(Limited,'matching_range_not_supported'):
            with source:pass
        stack.close();self.mode='ranges'
        stack,source=self.input()
        with self.assertRaisesRegex(Limited,'matching_source_changed'):
            with source:
                self.etag='"replaced"'
                try:self.fetch(source.url,100,131)
                except http.client.IncompleteRead:pass
        stack.close()

    def test_other_local_routes_cannot_fetch_an_arbitrary_source(self):
        stack,source=self.input()
        with source:
            before=source.stats()['bytes']
            with self.assertRaises(Exception):request.urlopen(source.url+'/other',timeout=5)
            self.assertEqual(source.stats()['bytes'],before)
        stack.close()

    def test_abandoned_range_is_closed_without_poisoning_other_seeks(self):
        self.data *= 1024
        self.video['size'] = len(self.data)
        self.reads.ledger.policies['premiumize'] = Policy(len(self.data)*2,1,0,0)
        self.reads.register(self.video)
        stack,source=self.input(ceiling=len(self.data))
        response=None
        try:
            with source:
                response=request.urlopen(source.url,timeout=5)
                self.assertEqual(self.fetch(source.url,1000,1031),self.data[1000:1032])
                closing=time.monotonic()
            self.assertLess(time.monotonic()-closing,2)
            self.assertIsNone(source.failure)
        finally:
            if response:response.close()
            stack.close()

    def worker_fixture(self,priority=300):
        import server
        self.server=server
        stack=ExitStack();self.addCleanup(stack.close)
        paths={name:self.root/name.lower() for name in ['AUDIO','MATCH','HLS','MATCHING']}
        for name,path in paths.items():
            path.mkdir();stack.enter_context(patch.object(server,name,path))
        stack.enter_context(patch.object(server,'MEDIA_READS',self.reads))
        stack.enter_context(patch.object(server.bounded_matching,'MAX_BYTES',256))
        stack.enter_context(patch.object(server.bounded_matching,'RESERVATION_BYTES',16))
        stack.enter_context(patch.object(server,'_dub_origin',return_value=None))
        stack.enter_context(patch.object(server.dubmux,'ffprobe_duration',return_value=3000.0))
        stack.enter_context(patch.object(server,'_file_layout',return_value=None))
        (server.AUDIO/'dub.m4a').write_bytes(b'cached-dub-audio')
        (server.AUDIO/'dub.json').write_text(json.dumps({'duration':3000.0}))
        self.key='dub__hq';server._jobs[self.key]={'status':'running','stage':'queued','priority':priority}
        self.dub={'id':'dub','url':'https://fixture.invalid/dub'}
        self.events=[]
        def prescreen(url,*args,**kwargs):
            self.events.append('sample')
            self.assertNotEqual(url,self.url,'unbounded upstream URL reached FFmpeg')
            self.fetch(url,16,31)
            return self.samples
        stack.enter_context(patch.object(server.dubmux,'prescreen',side_effect=prescreen))
        return stack

    def test_rejected_interactive_pair_never_requests_a_full_transfer(self):
        self.worker_fixture(priority=0)
        self.samples=[(i,0.0,1.0) for i in range(8)]
        with patch.object(self.server,'_full_reference',side_effect=AssertionError('full read before match')):
            self.server._prepare_worker(self.dub,self.video,self.key)
        self.assertEqual(self.server._jobs[self.key]['status'],'rejected')
        self.assertLess(self.reads.ledger.status()['providers']['premiumize']['reserved_bytes'],len(self.data))

    def test_background_constant_match_succeeds_with_no_full_video_allowance(self):
        self.worker_fixture()
        self.reads.ledger.policies['premiumize']=Policy(256,1,3600,0)
        self.samples=[(i,0.02,100.0) for i in range(8)]
        with patch.object(self.server,'_full_reference',side_effect=AssertionError('background full read')):
            self.server._prepare_worker(self.dub,self.video,self.key)
        self.assertEqual(self.server._jobs[self.key]['status'],'ready')
        self.assertEqual(json.loads(self.server._match_path(self.key).read_text())['verdict'],'accept')
        self.assertFalse(self.server._playback_result(self.key,self.server._jobs[self.key]['result'],self.url)['playback_available'])

    def test_full_admission_is_separate_and_durable_match_survives_deferral(self):
        self.worker_fixture(priority=0)
        self.samples=[(i,0.0,100.0) for i in range(8)]
        def full(*args):
            self.events.append('full')
            self.assertEqual(self.reads.ledger.status()['providers']['premiumize']['active'],0)
            raise Limited('daily_reservation_budget')
        with patch.object(self.server,'_full_reference',side_effect=full):
            self.server._prepare_worker(self.dub,self.video,self.key)
        self.assertEqual(self.events,['sample','full'])
        self.assertEqual(self.server._jobs[self.key]['status'],'limited')
        self.assertEqual(json.loads(self.server._match_path(self.key).read_text())['verdict'],'accept')

    def test_full_alignment_retry_reuses_prescreen_without_spending_again(self):
        self.worker_fixture()
        self.samples=[(i,float(i),100.0) for i in range(8)]
        with patch.object(self.server,'_full_reference',side_effect=Limited('daily_reservation_budget')):
            self.server._prepare_worker(self.dub,self.video,self.key)
            before=self.reads.ledger.status()['providers']['premiumize']['reserved_bytes']
            self.server._prepare_worker(self.dub,self.video,self.key)
        self.assertEqual(self.events,['sample'])
        self.assertEqual(self.reads.ledger.status()['providers']['premiumize']['reserved_bytes'],before)
        self.assertTrue(self.server._jobs[self.key]['matching']['checkpoint'])
        (self.server.AUDIO/'dub.m4a').write_bytes(b'changed audio')
        self.assertIsNone(self.server._matching_checkpoint(self.key,self.video,str(self.server.AUDIO/'dub.m4a')))

    def test_swallowed_read_failure_cannot_cache_a_rejection_or_start_video(self):
        self.worker_fixture(priority=0)
        def truncated(url,*args,**kwargs):
            try:self.fetch(url,0,511)
            except http.client.IncompleteRead:pass
            return [(i,0.0,1.0) for i in range(8)]
        with patch.object(self.server.dubmux,'prescreen',side_effect=truncated), \
             patch.object(self.server,'_full_reference',side_effect=AssertionError('full read fallback')):
            self.server._prepare_worker(self.dub,self.video,self.key)
        self.assertEqual(self.server._jobs[self.key]['status'],'limited')
        self.assertEqual(self.server._jobs[self.key]['stage'],'matching_byte_limit')
        self.assertFalse(self.server._match_path(self.key).exists())

    def test_piecewise_promotion_uses_local_audio_and_retains_rejection_gate(self):
        self.worker_fixture()
        self.samples=[(i,float(i),100.0) for i in range(8)]
        raw=self.root/'raw';raw.mkdir();(raw/'audio.mka').write_bytes(b'local reference')
        def full(*args):
            self.events.append('full')
            self.assertEqual(self.reads.ledger.status()['providers']['premiumize']['active'],0)
            return raw,self.url
        def align(*args,**kwargs):
            self.events.append('align')
            self.assertEqual(kwargs['hq_audio'],str(raw/'audio.mka'))
            return {'verdict':'reject:coverage'}
        with patch.object(self.server,'_full_reference',side_effect=full), \
             patch.object(self.server.align,'align',side_effect=align):
            self.server._prepare_worker(self.dub,self.video,self.key)
        self.assertEqual(self.events,['sample','full','align'])
        self.assertEqual(self.server._jobs[self.key]['status'],'rejected')
        self.assertEqual(json.loads(self.server._match_path(self.key).read_text())['verdict'],'reject:duration')


if __name__=='__main__':unittest.main()
