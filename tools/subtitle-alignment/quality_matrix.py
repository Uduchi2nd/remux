"""Private-fixture timing recovery matrix; no media URLs or subtitle text in reports.

Manifest: [{"title": "Example", "external": "translated.srt",
"reference": "embedded.srt", "additional_external": "optional-real.vtt"}].
External/reference must be a known synchronized pair from the same release.
Recovery is measured against external's original timing, not acoustic ground truth.
"""
import argparse,time,json,copy,subprocess,tempfile
from pathlib import Path
import worker
import numpy as np
import pysubs2
parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('manifest',type=Path)
parser.add_argument('output',type=Path)
args=parser.parse_args()
root=worker.ROOT; fixtures=args.manifest.resolve().parent;out=args.output
out.parent.mkdir(parents=True,exist_ok=True)
encoder=worker.Encoder();results=[];vectors={}
cases=json.loads(args.manifest.read_text())
for case in cases:
 label,extname,refname=case['title'],case['external'],case['reference']
 original=worker.parse((fixtures/extname).read_text());reference=worker.parse((fixtures/refname).read_text())
 t=time.monotonic();a=encoder.encode(original);b=encoder.encode(reference);similarities=a@b.T;encode_seconds=time.monotonic()-t
 vectors[label]=(original,reference,a,b)
 print(json.dumps({'title':label,'encoding_seconds':round(encode_seconds,2),'cues':[len(original),len(reference)]}),flush=True)
 for mode in ['unchanged','offset17','cut15to33','rate104']:
  external=copy.deepcopy(original)
  for i,s in enumerate(external):
   delta=0 if mode=='unchanged' else 17000 if mode=='offset17' else (15000 if i<len(external)//2 else 33000)
   if mode=='rate104':s.start=round(s.start*1.04);s.end=round(s.end*1.04)
   else:s.start+=delta;s.end+=delta
  t=time.monotonic()
  with tempfile.TemporaryDirectory(dir=root) as td:
   td=Path(td);external.save(str(td/'ext.srt'), keep_ssa_tags=True);reference.save(str(td/'ref.srt'))
   subprocess.run([str(root/'alass'),str(td/'ref.srt'),str(td/'ext.srt'),str(td/'out.srt')],check=True,timeout=90,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
   candidate=pysubs2.load(str(td/'out.srt'))
  report=worker.validate(external,reference,candidate,similarities)
  same=len(candidate)==len(original) and all(x.text==y.text for x,y in zip(original,candidate))
  assert same, 'Timing proposal changed source dialogue or cue count'
  errors=np.array([abs(x.start-y.start) for x,y in zip(candidate,original)])
  ends=np.array([abs(x.end-y.end) for x,y in zip(candidate,original)])
  result={'title':label,'case':mode,'encoding_seconds':round(encode_seconds,2),'alignment_seconds':round(time.monotonic()-t,2),'text_preserved':same,'recovery_median_ms':float(np.median(errors)),'recovery_p95_ms':float(np.quantile(errors,.95)),'recovery_max_ms':int(errors.max()),'end_p95_ms':float(np.quantile(ends,.95)),'report':report}
  results.append(result);out.write_text(json.dumps(results,indent=2));print(json.dumps(result),flush=True)
 if case.get('additional_external'):
  text=worker.parse((fixtures/case['additional_external']).read_text());t=time.monotonic()
  sim=encoder.encode(text)@b.T
  with tempfile.TemporaryDirectory(dir=root) as td:
   td=Path(td);text.save(str(td/'ext.srt'),keep_ssa_tags=True);reference.save(str(td/'ref.srt'));subprocess.run([str(root/'alass'),str(td/'ref.srt'),str(td/'ext.srt'),str(td/'out.srt')],check=True,timeout=90,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL);candidate=pysubs2.load(str(td/'out.srt'))
  result={'title':label,'case':'real-external','seconds':round(time.monotonic()-t,2),'report':worker.validate(text,reference,candidate,sim)};results.append(result);out.write_text(json.dumps(results,indent=2));print(json.dumps(result),flush=True)
# Cross-title negative: dialogue must not be accepted just because timing can be made similar.
labels=list(vectors)
for i in range(len(labels)-1):
 label,other=labels[i:i+2];external,_,a,_=vectors[label];_,reference,_,b=vectors[other]
 result={'title':label,'case':'wrong-title-reference','reference':other,'report':worker.validate(external,reference,external,a@b.T)}
 assert not result['report']['accepted'], 'Wrong-title reference was accepted'
 results.append(result);print(json.dumps(result),flush=True)
out.write_text(json.dumps(results,indent=2))
