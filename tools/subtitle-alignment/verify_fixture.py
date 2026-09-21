"""Private operator fixture test; fixture subtitle contents are never committed."""
import json
from pathlib import Path
from worker import Encoder, parse, validate

p=Path('/root/remux-alignment-runtime/fixtures')
external=parse((p/'external.srt').read_text())
reference=parse((p/'reference.srt').read_text())
candidate=parse((p/'external.srt.aligned.srt').read_text())
encoder=Encoder()
a,b=encoder.encode(external),encoder.encode(reference)
similarity=a@b.T
aligned=validate(external,reference,candidate,similarity)
uncorrected=validate(external,reference,external,similarity)
scrambled=validate(external,reference,candidate,similarity[:,::-1])
identity=validate(external,external,external,a@a.T)
assert aligned['accepted']
assert not uncorrected['accepted']
assert not scrambled['accepted']
assert identity['accepted']
assert all(x.text==y.text for x,y in zip(external,candidate))
report={'aligned':aligned,'uncorrected':uncorrected,'scrambled_reference':scrambled,'identity':identity}
(p/'verification.json').write_text(json.dumps(report,indent=2))
print(json.dumps(report,indent=2))
