"""Private subtitle-only alignment worker. No media URLs or cloud inference."""
import hashlib
from collections import Counter, OrderedDict
import copy
import hmac
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import numpy as np
import pysubs2

VERSION = "embedded-text-v5-reference-offset"
ROOT = Path(os.environ.get("ALIGN_RUNTIME", "/root/remux-alignment-runtime"))
MAX_BYTES = 2_000_000
MAX_CUES = 5000


def normalize(text):
    # File reads normalize CRLF automatically; HTTP request strings do not.
    # Canonical line endings preserve dialogue while avoiding false text changes.
    return text.replace('\r\n', '\n').replace('\r', '\n').lstrip('\ufeff')


def parse(text):
    if len(text.encode()) > MAX_BYTES:
        raise ValueError("subtitle too large")
    subs = pysubs2.SSAFile.from_string(normalize(text))
    if not 30 <= len(subs) <= MAX_CUES:
        raise ValueError("insufficient or excessive cues")
    if any(s.end <= s.start or s.start < 0 for s in subs):
        raise ValueError("invalid cue times")
    if any(a.start > b.start for a, b in zip(subs, subs[1:])):
        raise ValueError("unordered cues")
    return subs


class Encoder:
    def __init__(self):
        # Retained for offline comparisons only; production Engine never uses it.
        import onnxruntime as ort
        from tokenizers import Tokenizer
        self.tokenizer = Tokenizer.from_file(str(ROOT / "model/tokenizer.json"))
        self.tokenizer.enable_truncation(max_length=128)
        self.tokenizer.no_padding()
        self.cache = OrderedDict()
        opts = ort.SessionOptions()
        opts.intra_op_num_threads = 2
        opts.inter_op_num_threads = 1
        self.session = ort.InferenceSession(str(ROOT / "model/model.onnx"), opts,
                                           providers=["CPUExecutionProvider"])

    def encode(self, subs):
        # Context disambiguates short dialogue such as 'yes' or 'Mr. Pei'.
        texts = [" ".join(s.plaintext for s in subs[max(0, i-1):i+2])
                 for i in range(len(subs))]
        key = hashlib.sha256(json.dumps(texts, ensure_ascii=False).encode()).hexdigest()
        if key in self.cache:
            self.cache.move_to_end(key)
            return self.cache[key]
        encoded = self.tokenizer.encode_batch(texts)
        # Similar lengths in each batch avoid running attention on padding.
        order = sorted(range(len(texts)), key=lambda i: len(encoded[i].ids))
        vectors = []
        names = {x.name for x in self.session.get_inputs()}
        for start in range(0, len(texts), 32):
            batch = [encoded[i] for i in order[start:start+32]]
            width = max(len(x.ids) for x in batch)
            feed = {"input_ids": np.array([x.ids + [0]*(width-len(x.ids)) for x in batch], dtype=np.int64),
                    "attention_mask": np.array([x.attention_mask + [0]*(width-len(x.ids)) for x in batch], dtype=np.int64),
                    "token_type_ids": np.array([x.type_ids + [0]*(width-len(x.ids)) for x in batch], dtype=np.int64)}
            output = self.session.run(None, {k:v for k,v in feed.items() if k in names})[0]
            mask = feed["attention_mask"][..., None]
            pooled = (output * mask).sum(1) / np.maximum(mask.sum(1), 1)
            pooled /= np.maximum(np.linalg.norm(pooled, axis=1, keepdims=True), 1e-9)
            vectors.append(pooled.astype(np.float32))
        result = np.concatenate(vectors)[np.argsort(order)]
        self.cache[key] = result
        while len(self.cache) > 6:
            self.cache.popitem(last=False)
        return result


def exact_validation(original, reference, candidate):
    """Fast, conservative gate for substantially identical dialogue editions."""
    if len(candidate) != len(original) or any(a.text != b.text for a,b in zip(original,candidate)):
        return None
    if any(s.start < 0 or s.end <= s.start for s in candidate):
        return None
    if any(a.start > b.start for a,b in zip(candidate,candidate[1:])):
        return None
    # Only whitespace is normalized; fuzzy matches require semantic validation.
    ext = [' '.join(s.plaintext.split()) for s in original]
    ref = [' '.join(s.plaintext.split()) for s in reference]
    ec, rc = Counter(ext), Counter(ref)
    lookup = {text:i for i,text in enumerate(ref) if rc[text] == 1}
    anchors = [(i,lookup[text]) for i,text in enumerate(ext)
               if len(text) >= 12 and ec[text] == 1 and text in lookup]
    if len(anchors) < max(40, len(original)*.6):
        return None
    good = [abs(candidate[i].start-reference[j].start) <= 250
            and abs(candidate[i].end-reference[j].end) <= 500 for i,j in anchors]
    bins = []
    duration = max(s.end for s in original)
    for n in range(10):
        ids = [k for k,(i,j) in enumerate(anchors)
               if n/10 <= original[i].start/duration < (n+1)/10]
        bins.append(float(np.mean([good[k] for k in ids])) if len(ids) >= 3 else None)
    if np.mean(good) < .99 or sum(x is not None for x in bins) < 9:
        return None
    if any(x is not None and x < .95 for x in bins):
        return None
    return {"accepted": True, "reason": "exact dialogue validated", "method": "exact",
            "anchors": len(anchors), "anchor_fraction": round(len(anchors)/len(original),3),
            "within_250ms": round(float(np.mean(good)),3), "coverage_bins": bins}


def validate(original, reference, candidate, similarities):
    if len(candidate) != len(original) or any(a.text != b.text for a,b in zip(original,candidate)):
        return {"accepted": False, "reason": "content changed"}
    if any(s.end <= s.start or s.start < 0 for s in candidate):
        return {"accepted": False, "reason": "invalid output times"}
    if any(a.start > b.start for a,b in zip(candidate,candidate[1:])):
        return {"accepted": False, "reason": "non-monotonic output"}
    # Find semantic anchors independently of the proposed timing correction.
    matches = np.argmax(similarities, axis=1)
    scores = similarities[np.arange(len(original)), matches]
    anchors = []
    for i, j in enumerate(matches):
        if scores[i] < .72 or len(original[i].plaintext.strip()) < 8:
            continue
        # Repeated phrases far apart must not establish alignment confidence.
        row = similarities[i].copy()
        row[max(0,j-3):j+4] = -1
        if scores[i] - row.max() < .035:
            continue
        anchors.append((i, int(j)))
    if len(anchors) < max(25, len(original)*.08):
        return {"accepted": False, "reason": "too few distinct dialogue anchors", "anchors":len(anchors)}
    residuals = np.array([(candidate[i].start-reference[j].start)/1000 for i,j in anchors])
    good = np.abs(residuals) <= 2.5
    duration = original[-1].end
    bins = []
    for n in range(10):
        ids = [k for k,(i,j) in enumerate(anchors) if n/10 <= original[i].start/duration < (n+1)/10]
        if not ids:
            bins.append(None)
        else:
            bins.append(float(np.mean(good[ids])))
    represented = sum(x is not None for x in bins)
    accepted = bool(good.mean() >= .85 and represented >= 8
                    and all(x is None or x >= .6 for x in bins))
    return {"accepted":accepted,"reason":"validated" if accepted else "dialogue timing disagreement",
            "anchors":len(anchors),"anchor_fraction":round(len(anchors)/len(original),3),
            "within_2_5s":round(float(good.mean()),3),
            "median_abs_error_s":round(float(np.median(np.abs(residuals))),3),
            "p90_abs_error_s":round(float(np.quantile(np.abs(residuals),.9)),3),
            "coverage_bins":bins}


def structural_validation(original, candidate):
    """Integrity checks only; these do not verify dialogue meaning or sync."""
    if len(candidate) != len(original) or any(a.text != b.text for a,b in zip(original,candidate)):
        return {"accepted": False, "reason": "content changed", "method": "alass-only"}
    if any(s.start < 0 or s.end <= s.start for s in candidate):
        return {"accepted": False, "reason": "invalid output times", "method": "alass-only"}
    if any(a.start > b.start for a,b in zip(candidate,candidate[1:])):
        return {"accepted": False, "reason": "non-monotonic output", "method": "alass-only"}
    return {"accepted": True, "reason": "structural checks passed",
            "method": "alass-only", "semantic_validation": False}


class Engine:
    def __init__(self):
        self.lock = threading.Lock()
        (ROOT / "cache").mkdir(exist_ok=True)

    def align(self, external, reference):
        external, reference = normalize(external), normalize(reference)
        # Exact track fingerprint: reviewed offsets never spill to other releases.
        ref = parse(reference)
        fingerprint = hashlib.sha256(json.dumps(
            [[s.start, s.end, s.text] for s in ref], ensure_ascii=False,
            separators=(',', ':')).encode()).hexdigest()
        overrides_path = ROOT / 'reference-timing-overrides.json'
        overrides = json.loads(overrides_path.read_text()) if overrides_path.exists() else {}
        if not isinstance(overrides, dict):
            raise ValueError('invalid reference timing overrides')
        shift = overrides.get(fingerprint, 0)
        if type(shift) is not int or abs(shift) > 120000:
            raise ValueError('invalid reference offset')
        if shift:
            for cue in ref:
                cue.start += shift; cue.end += shift
            if any(c.start < 0 or c.end <= c.start for c in ref):
                raise ValueError('reference offset produces invalid times')
        key = hashlib.sha256((VERSION+'\0'+external+'\0'+reference+'\0'+str(shift)).encode()).hexdigest()
        cached = ROOT / "cache" / (key+'.json')
        with self.lock:
            if cached.exists() and time.time()-cached.stat().st_mtime < 14*86400:
                return json.loads(cached.read_text())
            original = parse(external)
            with tempfile.TemporaryDirectory(dir=ROOT) as temp:
                p=Path(temp)
                original.save(str(p/'external.srt'), keep_ssa_tags=True)
                ref.save(str(p/'reference.srt'))
                subprocess.run([str(ROOT/'alass'),str(p/'reference.srt'),str(p/'external.srt'),str(p/'result.srt')],
                               check=True,timeout=20,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
                candidate=pysubs2.load(str(p/'result.srt'))
                # ALASS supplies timestamps; the original supplies all dialogue.
                # Validate its serialized cue mapping before copying any times.
                serialized=pysubs2.load(str(p/'external.srt'))
                if len(candidate)!=len(original) or any(a.text!=b.text for a,b in zip(serialized,candidate)):
                    raise ValueError('alignment changed cue mapping')
                timed=copy.deepcopy(original)
                for cue, proposed in zip(timed,candidate):
                    cue.start, cue.end = proposed.start, proposed.end
                candidate=timed
            report=structural_validation(original,candidate)
            report["reference_offset_ms"] = shift
            result={"version":VERSION,"key":key,"report":report}
            if report['accepted']:
                result['subtitle']=candidate.to_string('srt', keep_ssa_tags=True)
            staging=cached.with_suffix('.tmp')
            staging.write_text(json.dumps(result));staging.replace(cached)
            # Bounded cache: keep at most 200 results and expire after 14 days.
            files=sorted((ROOT/'cache').glob('*.json'),key=lambda f:f.stat().st_mtime,reverse=True)
            for n,f in enumerate(files):
                if n>=200 or time.time()-f.stat().st_mtime>14*86400:f.unlink(missing_ok=True)
            return result


def serve():
    token = (ROOT/'token').read_text().strip()
    engine = Engine()
    slots = threading.BoundedSemaphore(2)
    class Handler(BaseHTTPRequestHandler):
        def log_message(self,*args):pass
        def setup(self):
            super().setup()
            self.connection.settimeout(15)
        def do_POST(self):
            if self.path!='/align' or not hmac.compare_digest(self.headers.get('Authorization',''), 'Bearer '+token):
                self.send_error(403);return
            try: length=int(self.headers.get('Content-Length','0'))
            except ValueError: self.send_error(400);return
            if not 0<length<2*MAX_BYTES+4096:self.send_error(413);return
            if not slots.acquire(blocking=False):self.send_error(503);return
            try:
                data=json.loads(self.rfile.read(length))
                if not isinstance(data.get('external'),str) or not isinstance(data.get('reference'),str):
                    raise ValueError('subtitle text required')
                result=engine.align(data['external'],data['reference'])
                body=json.dumps(result).encode()
                self.send_response(200);self.send_header('Content-Type','application/json')
                self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
            except (ValueError,KeyError,subprocess.SubprocessError):self.send_error(422)
            except Exception:self.send_error(500)
            finally:slots.release()
    ThreadingHTTPServer((os.environ.get('ALIGN_BIND','10.10.10.1'),8791),Handler).serve_forever()


if __name__=='__main__':
    import sys
    if len(sys.argv)==3:
        result=Engine().align(Path(sys.argv[1]).read_text(),Path(sys.argv[2]).read_text())
        print(json.dumps({k:v for k,v in result.items() if k!='subtitle'},indent=2))
        if 'subtitle' in result:Path(sys.argv[1]+'.aligned.srt').write_text(result['subtitle'])
    else:serve()
