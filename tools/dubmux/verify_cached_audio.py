"""Run in candidate image --network=none, using a synthetic completed raw cache."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
from unittest.mock import patch

with tempfile.TemporaryDirectory() as directory:
    os.environ['DUBMUX_DATA']=directory
    import server
    import dubmux
    raw=server._raw_dir('fixture-hq')
    raw.mkdir(parents=True)
    def run(args):
        subprocess.run(['ffmpeg','-v','error','-nostdin','-y',*args],check=True,capture_output=True,timeout=30)
    original=Path(directory)/'original.mkv'
    run(['-f','lavfi','-i','color=size=64x64:rate=10:duration=6','-f','lavfi','-i','anoisesrc=color=pink:seed=123:duration=6',
         '-c:v','libx264','-c:a','aac',str(original)])
    run(['-i',str(original),'-map','0:v:0','-map','0:a?','-c','copy','-f','hls','-hls_time','2',
         '-hls_playlist_type','vod',str(raw/'index.m3u8')])
    run(['-i',str(raw/'index.m3u8'),'-vn','-c:a','copy',str(raw/'audio.mka')])
    original_pcm=dubmux.decode(str(original),1,3)
    cached_pcm=dubmux.decode(str(raw/'audio.mka'),1,3)
    lag,_,ratio=dubmux.xcorr_lag(original_pcm,cached_pcm,0.5)
    assert abs(lag)<0.04 and ratio>10, 'cached audio changed the reference clock'
    (raw/'.done').write_text('fixture')
    (raw/'.video_ok').write_text('fixture')
    run(['-i',str(raw/'audio.mka'),'-c:a','copy',str(server.AUDIO/'fixture-dub.m4a')])
    duration=dubmux.ffprobe_duration(str(raw/'index.m3u8'))
    (server.AUDIO/'fixture-dub.json').write_text(json.dumps({'duration':duration,'source':'fixture'}))
    key='fixture-dub__fixture-hq'
    server._jobs[key]={'priority':100,'stage':'queued','status':'running'}
    sampled=[]
    def prescreen(source,*_args,**_kwargs):
        sampled.append(source)
        assert source==str(raw/'audio.mka'), 'remote audio used despite completed cache'
        return [(n,0.0,100.0) for n in range(8)]
    with patch.object(dubmux,'resolve_url',side_effect=AssertionError('remote resolver called')), \
         patch.object(server.MEDIA_READS,'acquire',side_effect=AssertionError('paid lease requested')), \
         patch.object(dubmux,'cmd_extract',side_effect=AssertionError('cached dub re-extracted')), \
         patch.object(server,'_dub_origin',return_value=None), \
         patch.object(dubmux,'prescreen',side_effect=prescreen):
        server._prepare_worker({'id':'fixture-dub','url':'https://offline.invalid/dub'},
                              {'id':'fixture-hq','url':'https://offline.invalid/release.mkv'},key)
    assert sampled and server._jobs[key]['status']=='ready', server._jobs[key].get('status')
    assert all(v['reserved_bytes']==0 for v in server.MEDIA_READS.ledger.status()['providers'].values())
    print(json.dumps({'cached_audio_matched':True,'reference_clock_error_seconds':round(float(lag),5),'provider_reads':0,'provider_reservations':0,'external_network':'disabled'}))
