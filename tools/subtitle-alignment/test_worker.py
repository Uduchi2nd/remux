import copy
import unittest
import numpy as np
import pysubs2
from worker import normalize, parse, validate


def cues():
    result = pysubs2.SSAFile()
    for i in range(100):
        result.append(pysubs2.SSAEvent(start=i*5000, end=i*5000+2000,
                                       text=f'Distinct dialogue number {i}'))
    return result


class ValidationTests(unittest.TestCase):
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
