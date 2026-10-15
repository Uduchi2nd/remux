import copy
import unittest
import tempfile
from pathlib import Path
from unittest.mock import patch
import numpy as np
import pysubs2
import worker
from worker import normalize, parse, validate, exact_validation, structural_validation


def cues():
    result = pysubs2.SSAFile()
    for i in range(100):
        result.append(pysubs2.SSAEvent(start=i*5000, end=i*5000+2000,
                                       text=f'Distinct dialogue number {i}'))
    return result


class ValidationTests(unittest.TestCase):
    def test_production_engine_never_constructs_language_model(self):
        def propose(args, **kwargs):
            self.assertEqual(kwargs['timeout'], 20)
            source=pysubs2.load(args[2])
            source.shift(ms=17000)
            source.save(args[3],keep_ssa_tags=True)
        with tempfile.TemporaryDirectory() as directory, \
             patch.object(worker,'ROOT',Path(directory)), \
             patch.object(worker,'Encoder',side_effect=AssertionError('model called')), \
             patch.object(worker.subprocess,'run',side_effect=propose):
            s=cues();result=worker.Engine().align(s.to_string('srt'),s.to_string('srt'))
            self.assertEqual(result['report']['method'],'alass-only')
            self.assertFalse(result['report']['semantic_validation'])
            self.assertTrue(result['report']['timing_changed'])
            aligned=parse(result['subtitle'])
            self.assertEqual(aligned[20].start,s[20].start+17000)
            self.assertEqual([c.text for c in aligned],[c.text for c in s])

    def test_identical_timing_is_not_marked_changed(self):
        def propose(args, **kwargs):
            pysubs2.load(args[2]).save(args[3])
        with tempfile.TemporaryDirectory() as d, patch.object(worker,'ROOT',Path(d)), patch.object(worker.subprocess,'run',side_effect=propose):
            s=cues().to_string('srt');result=worker.Engine().align(s,s)
            self.assertFalse(result['report']['timing_changed'])

    def test_alass_formatting_normalization_keeps_original_dialogue_and_styles(self):
        def propose(args, **kwargs):
            source=pysubs2.load(args[2])
            self.assertIn(' {\\i0}', source[12].text)
            source[12].text=source[12].text.replace(' {\\i0}', '{\\i0}')
            source.shift(ms=17000)
            source.save(args[3], keep_ssa_tags=True)
        with tempfile.TemporaryDirectory() as d, patch.object(worker,'ROOT',Path(d)), \
             patch.object(worker.subprocess,'run',side_effect=propose), \
             patch.object(worker,'Encoder',side_effect=AssertionError('model called')):
            source=cues()
            source[12].text=r'{\i1}Distinct dialogue number 12 {\i0}'
            result=worker.Engine().align(source.to_string('srt',keep_ssa_tags=True), source.to_string('srt'))
            self.assertTrue(result['report']['accepted'])
            self.assertTrue(result['report']['timing_changed'])
            aligned=parse(result['subtitle'])
            self.assertEqual([cue.text for cue in aligned],[cue.text for cue in source])
            self.assertEqual(aligned[12].start, source[12].start+17000)

    def test_alass_dialogue_changes_and_missing_cues_still_reject(self):
        for change in ['word','punctuation','missing','order']:
            with self.subTest(change=change):
                def propose(args, **kwargs):
                    source=pysubs2.load(args[2])
                    if change=='word': source[12].text='Different dialogue number 12'
                    if change=='punctuation': source[12].text+='!'
                    if change=='missing': del source[12]
                    if change=='order': source[12].text,source[13].text=source[13].text,source[12].text
                    source.save(args[3],keep_ssa_tags=True)
                with tempfile.TemporaryDirectory() as d, patch.object(worker,'ROOT',Path(d)), \
                     patch.object(worker.subprocess,'run',side_effect=propose):
                    source=cues().to_string('srt')
                    with self.assertRaisesRegex(ValueError,'cue mapping'):
                        worker.Engine().align(source,source)

    def test_raw_srt_literal_line_breaks_survive_output_serialization(self):
        def propose(args, **kwargs):
            source=pysubs2.load(args[2])
            source.shift(ms=17000)
            source.save(args[3],keep_ssa_tags=True)
        with tempfile.TemporaryDirectory() as d, patch.object(worker,'ROOT',Path(d)), \
             patch.object(worker.subprocess,'run',side_effect=propose):
            source=cues().to_string('srt').replace('Distinct dialogue number 0',
                r'\NDistinct dialogue number 0\N',1)
            result=worker.Engine().align(source,source)
            self.assertTrue(result['report']['accepted'])
            self.assertIn(r'\NDistinct dialogue number 0\N',result['subtitle'])
            self.assertEqual(worker.TIMING_LINE.sub('<TIMES>',source),
                             worker.TIMING_LINE.sub('<TIMES>',result['subtitle']))
            self.assertEqual([cue.text for cue in parse(source)],
                             [cue.text for cue in parse(result['subtitle'])])

    def test_webvtt_cue_ids_and_settings_survive_retiming(self):
        source=('WEBVTT\n\nscene-caption\n'
                '00:01.000 --> 00:02.000 align:start position:20%\n'
                '<i>Original words</i>\n\n')
        timed=pysubs2.SSAFile.from_string(source)
        timed.shift(ms=17000)
        result=worker.retime_original(source,timed)
        self.assertEqual(result,source.replace('00:01.000 --> 00:02.000',
                                              '00:00:18.000 --> 00:00:19.000'))

    def test_structural_checks_reject_corrupt_output(self):
        s=cues()
        for change in ['text','negative','duration','order']:
            bad=copy.deepcopy(s)
            if change=='text':bad[5].text='Modified'
            if change=='negative':bad[0].start=-1
            if change=='duration':bad[5].end=bad[5].start
            if change=='order':bad[5].start=bad[4].start-1
            self.assertFalse(structural_validation(s,bad)['accepted'])

    def test_structural_acceptance_does_not_claim_semantic_confidence(self):
        s=cues();result=structural_validation(s,s)
        self.assertTrue(result['accepted'])
        self.assertFalse(result['semantic_validation'])

    def test_exact_dialogue_fast_path(self):
        s=cues()
        self.assertTrue(exact_validation(s,s,s)['accepted'])

    def test_exact_path_rejects_bad_timing_and_missing_tail(self):
        s=cues(); bad=copy.deepcopy(s)
        for cue in bad[-10:]: cue.start+=20000; cue.end+=20000
        self.assertIsNone(exact_validation(s,s,bad))
        ref=copy.deepcopy(s)
        for cue in ref[-20:]: cue.text='Different dialogue'
        self.assertIsNone(exact_validation(s,ref,s))

    def test_exact_path_rejects_repeated_dialogue(self):
        s=cues()
        for cue in s:cue.text='A repeated line of dialogue'
        self.assertIsNone(exact_validation(s,s,s))

    def test_position_tags_survive_srt_roundtrip(self):
        s=cues();s[4].text=r'{\an8}Positioned dialogue\NSecond line'
        roundtrip=parse(s.to_string('srt',keep_ssa_tags=True))
        self.assertEqual(s[4].text,roundtrip[4].text)
        self.assertTrue(exact_validation(s,s,roundtrip)['accepted'])

    def test_http_crlf_and_file_lf_preserve_identical_dialogue(self):
        text = cues().to_string('vtt')
        wire = '\ufeff' + text.replace('\n', '\r\n')
        self.assertEqual(normalize(wire), text)
        self.assertEqual([s.text for s in parse(wire)], [s.text for s in parse(text)])
        self.assertTrue(validate(parse(wire), parse(text), parse(text), np.eye(100))['accepted'])

    def test_identity_is_accepted(self):
        s = cues()
        self.assertTrue(validate(s,s,s,np.eye(100))['accepted'])

    def test_changing_offset_is_accepted(self):
        original = cues()
        reference = copy.deepcopy(original)
        for i, cue in enumerate(reference):
            cue.start += 15000 if i < 50 else 33000
            cue.end += 15000 if i < 50 else 33000
        self.assertFalse(validate(original,reference,original,np.eye(100))['accepted'])
        self.assertTrue(validate(original,reference,reference,np.eye(100))['accepted'])

    def test_wrong_episode_is_rejected(self):
        s=cues()
        self.assertFalse(validate(s,s,s,np.full((100,100),.4))['accepted'])

    def test_repeated_dialogue_cannot_establish_confidence(self):
        s=cues()
        self.assertFalse(validate(s,s,s,np.full((100,100),.99))['accepted'])

    def test_bad_tail_is_rejected_despite_good_overall_score(self):
        s=cues(); bad=copy.deepcopy(s)
        for cue in bad[-10:]: cue.start+=20000; cue.end+=20000
        self.assertFalse(validate(s,s,bad,np.eye(100))['accepted'])

    def test_changed_dialogue_is_rejected(self):
        s=cues(); bad=copy.deepcopy(s);bad[50].text='changed'
        self.assertFalse(validate(s,s,bad,np.eye(100))['accepted'])

    def test_invalid_or_tiny_input_is_rejected(self):
        with self.assertRaises(ValueError):parse('1\n00:00:01,000 --> 00:00:02,000\nHello\n')


if __name__ == '__main__':unittest.main()
