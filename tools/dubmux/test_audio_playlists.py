"""Audio selection and byte-range plans; no external requests."""
import unittest
from unittest.mock import MagicMock, patch
import dubmux


class AudioPlaylistsTest(unittest.TestCase):
    def test_ignored_segment_ranges_do_not_download_the_full_resource(self):
        response=MagicMock()
        response.status=200
        response.headers={"Content-Length":"1000000000"}
        response.read.side_effect=AssertionError('full media body read')
        context=MagicMock()
        context.__enter__.return_value=response
        with patch.object(dubmux.urllib.request,'urlopen',return_value=context):
            with self.assertRaises(dubmux.PlaylistUnavailable):
                dubmux.http_get('https://fixture.invalid/large.ts',rng=(10,20))
        response.read.assert_not_called()

    def select(self, master, playlists):
        pages = {'https://fixture.invalid/master.m3u8':master, **{
            'https://fixture.invalid/'+k:v for k,v in playlists.items()}}
        def get(url, **options):
            self.assertEqual(options['max_bytes'],512*1024)
            return pages[url].encode()
        with patch.object(dubmux,'http_get',side_effect=get):
            return dubmux.parse_media_playlist('https://fixture.invalid/master.m3u8')

    def test_vietnamese_audio_is_selected_with_quoted_commas_and_matching_group(self):
        master='''#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="other",LANGUAGE="vi",URI="wrong.m3u8"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="dub",LANGUAGE="en",DEFAULT=YES,URI="english.m3u8"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="dub",LANGUAGE="vi",NAME="Vietnamese, dub",URI="vi/audio.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=9000000,AUDIO="dub",CODECS="avc1.640028,mp4a.40.2"
video.m3u8
'''
        url,segs=self.select(master,{'vi/audio.m3u8':'#EXTM3U\n#EXTINF:6,\nseg0.aac\n#EXT-X-ENDLIST\n'})
        self.assertEqual(url,'https://fixture.invalid/vi/audio.m3u8')
        self.assertEqual(segs,[('https://fixture.invalid/vi/seg0.aac',None)])

    def test_inband_default_is_not_replaced_by_an_unrelated_language(self):
        master='''#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="a",LANGUAGE="en",DEFAULT=YES
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="a",LANGUAGE="es",URI="spanish.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=1000000,AUDIO="a"
mixed.m3u8
'''
        url,_=self.select(master,{'mixed.m3u8':'#EXTM3U\n#EXTINF:6,\nvideo.ts\n'})
        self.assertTrue(url.endswith('/mixed.m3u8'))

    def test_audio_only_variant_and_fragment_initialization_are_kept(self):
        master='''#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=3000000,CODECS="avc1.640028,mp4a.40.2"
video.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=160000,CODECS="mp4a.40.2"
audio.m3u8
'''
        url,segs=self.select(master,{'audio.m3u8':'''#EXTM3U
#EXT-X-MAP:URI="audio.mp4",BYTERANGE="20@0"
#EXT-X-BYTERANGE:100@20
audio.mp4
#EXT-X-BYTERANGE:100
audio.mp4
'''})
        self.assertTrue(url.endswith('/audio.m3u8'))
        self.assertEqual([r for _,r in segs],[(0,20),(20,100),(120,100)])

    def test_nested_masters_cycles_and_encryption_are_bounded(self):
        with self.assertRaises(dubmux.PlaylistUnavailable):
            self.select('#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nmaster.m3u8\n',{})
        with self.assertRaises(dubmux.PlaylistUnavailable):
            self.select('#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI="key"\nsegment.ts\n',{})

    def test_mediaflow_wrapper_keeps_the_origin_for_relative_audio_urls(self):
        wrapped='https://proxy.invalid/proxy/stream?d=https%3A%2F%2Forigin.invalid%2Fmaster.m3u8&api_password=fixture'
        target=dubmux._join(wrapped,'audio/vi.m3u8')
        self.assertIn('origin.invalid%2Faudio%2Fvi.m3u8',target)
        self.assertIn('api_password=fixture',target)


if __name__=='__main__': unittest.main()
