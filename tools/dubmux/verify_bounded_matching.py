"""Production-image check with generated media, loopback HTTP, and real FFmpeg."""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import patch
import json
import os
import re
import subprocess
import threading
from io import BytesIO
from bounded_matching import MatchingInput,INPUT_OPTIONS

with TemporaryDirectory() as directory:
    root=Path(directory)
    os.environ['DUBMUX_DATA']=str(root/'data')
    import server
    import dubmux
    from provider_budget import Policy,Limited,MediaReads
    def run(args):
        subprocess.run(['ffmpeg','-v','error','-nostdin','-y',*args],check=True,
                       capture_output=True,timeout=90)
    hq=root/'fixture.mkv'
    run(['-f','lavfi','-i','testsrc2=size=320x180:rate=25',
         '-f','lavfi','-i','anoisesrc=sample_rate=48000:color=pink:seed=123',
         '-t','60','-c:v','libx264','-preset','ultrafast','-threads','1','-g','50',
         '-b:v','40M','-minrate','40M','-maxrate','40M','-bufsize','80M',
         '-x264-params','nal-hrd=cbr:force-cfr=1','-c:a','aac','-b:a','96k',str(hq)])
    for name in ['good','wrong']:
        if name=='good':run(['-i',str(hq),'-map','0:a:0','-c:a','copy',str(server.AUDIO/(name+'.m4a'))])
        else:run(['-f','lavfi','-i','anoisesrc=sample_rate=48000:color=pink:seed=999:duration=60',
                  '-c:a','aac','-b:a','96k',str(server.AUDIO/(name+'.m4a'))])
        (server.AUDIO/(name+'.json')).write_text(json.dumps({'duration':60.0}))
    class Origin(BaseHTTPRequestHandler):
        def log_message(self,*args):pass
        def do_GET(self):
            if self.path=='/escaped':
                escaped.append(True);self.send_error(404);return
            if self.path=='/playlist.mkv':
                data=playlist;size=len(data);handle=lambda:BytesIO(data)
            else:
                path=server.AUDIO/'good.m4a' if self.path=='/audio.m4a' else hq
                size=path.stat().st_size;handle=lambda:path.open('rb')
            match=re.fullmatch(r'bytes=(\d+)-(\d+)',self.headers.get('Range',''))
            if not match:self.send_error(400);return
            start,end=map(int,match.groups());end=min(end,size-1)
            self.send_response(206);self.send_header('Content-Range',f'bytes {start}-{end}/{size}')
            self.send_header('Content-Length',str(end-start+1));self.send_header('ETag','"fixture"');self.end_headers()
            with handle() as f:
                f.seek(start);self.wfile.write(f.read(end-start+1))
    origin=ThreadingHTTPServer(('127.0.0.1',0),Origin)
    escaped=[]
    playlist=(f'#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nhttp://127.0.0.1:{origin.server_port}/escaped\n#EXT-X-ENDLIST\n').encode()
    # Leave enough bounded probe allowance to reach the format guard, rather
    # than exhausting a tiny file's one-file byte ceiling on repeated reads.
    playlist += b'# fixture padding\n' * 262144
    threading.Thread(target=origin.serve_forever,daemon=True).start()
    original_prescreen=dubmux.prescreen
    original_probe=dubmux.ffprobe_duration
    original_input=server.bounded_matching.MatchingInput
    active_inputs=[]
    def captured_input(*args,**kwargs):
        value=original_input(*args,**kwargs)
        original_fail=value.fail
        def traced_fail(exc):
            print(json.dumps({'phase':'reader-deferred','exception':type(exc).__name__,'detail':str(exc)}),flush=True)
            original_fail(exc)
        value.fail=traced_fail
        active_inputs.append(value)
        return value
    def traced_probe(*args,**kwargs):
        value=original_probe(*args,**kwargs)
        print(json.dumps({'phase':'duration-probe','bytes':active_inputs[-1].stats()['bytes'],
                          'options':kwargs.get('input_options'),'duration':value}),flush=True)
        return value
    def short_fixture(*args,**kwargs):
        # Small windows keep this generated-media CI check quick; production
        # retains its existing eight 60-second windows and acceptance gates.
        return original_prescreen(*args,n=5,span=1.0,**kwargs)
    try:
        reports=[]
        for name in ['good','wrong']:
            # Each isolated policy can afford matching but not this whole video.
            server.MEDIA_READS=MediaReads(root/('ledger-'+name))
            for p in server.MEDIA_READS.ledger.policies:
                allowance=(256 if name=='good' else 64)*1024**2
                server.MEDIA_READS.ledger.policies[p]=Policy(allowance,3,3600,0)
            provider='premiumize'
            video={'id':name+'-hq','url':f'http://127.0.0.1:{origin.server_port}/fixture.mkv',
                   'provider':provider,'size':hq.stat().st_size}
            server.MEDIA_READS.register(video)
            key=name+'__'+video['id']
            server._jobs[key]={'status':'running','stage':'queued','priority':300}
            with patch.object(server,'_dub_origin',return_value=None), \
                 patch.object(dubmux,'prescreen',side_effect=short_fixture), \
                 patch.object(dubmux,'ffprobe_duration',side_effect=traced_probe), \
                 patch.object(server.bounded_matching,'MatchingInput',side_effect=captured_input), \
                 patch.object(server,'_full_reference',side_effect=AssertionError('fixture must not download full video')):
                server._prepare_worker({'id':name,'url':'https://fixture.invalid/dub'},video,key)
            job=server._jobs[key]
            assert job['status'] in (['ready'] if name=='good' else ['rejected','limited']),job
            result=job.get('result') or {}
            if name=='good':assert abs(result['lag'])<.05,result
            if job['status']=='limited':
                assert not server._match_path(key).exists(),'partial matching persisted a verdict'
                assert job['stage'] in ('daily_reservation_budget','matching_byte_limit','matching_time_limit'),job
            assert job['matching']['bytes']<hq.stat().st_size,job['matching']
            full_blocked=False
            try:server.MEDIA_READS.check(video['id'],video['url'])
            except Limited:full_blocked=True
            assert full_blocked,'Fixture policy unexpectedly admitted full video'
            reports.append({'fixture':name,'status':job['status'],'stage':job['stage'],'file_bytes':hq.stat().st_size,
                            'matching':job['matching'],'lag_seconds':result.get('lag'),
                            'confident_windows':(result.get('prescreen') or {}).get('confident'),
                            'full_video_not_admitted':full_blocked,
                            'reserved_bytes':server.MEDIA_READS.ledger.status()['providers'][provider]['reserved_bytes']})
            print(json.dumps(reports[-1]),flush=True)
        with MatchingInput(f'http://127.0.0.1:{origin.server_port}/audio.m4a',
                           (server.AUDIO/'good.m4a').stat().st_size,lambda total:None,lambda url:None,request_gap=0) as media:
            duration=dubmux.ffprobe_duration(media.url,input_options=INPUT_OPTIONS)
            assert abs(duration-60)<.1,duration
        playlist_input=MatchingInput(f'http://127.0.0.1:{origin.server_port}/playlist.mkv',len(playlist),
                                     lambda total:None,lambda url:None,request_gap=0)
        # Force HLS detection so this specifically exercises our format guard.
        playlist_input.path += '.m3u8'
        with playlist_input as media:
            try:dubmux.ffprobe_duration(media.url,input_options=INPUT_OPTIONS)
            except RuntimeError as exc:
                assert 'whitelist' in str(exc),str(exc)
            else:raise AssertionError('nested playlist was accepted')
        assert not escaped,'FFmpeg bypassed bounded input through a child URL'
        print(json.dumps({'external_network':'disabled','real_ffmpeg_matching':reports,
                          'mov_duration':duration,'nested_playlist_blocked':not escaped}),flush=True)
    finally:
        origin.shutdown();origin.server_close()
