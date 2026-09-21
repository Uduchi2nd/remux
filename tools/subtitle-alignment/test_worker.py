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
            aligned=parse(result['subtitle'])
            self.assertEqual(aligned[20].start,s[20].start+17000)
            self.assertEqual([c.text for c in aligned],[c.text for c in s])

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
