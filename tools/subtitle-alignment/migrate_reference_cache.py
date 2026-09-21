"""One-time v1->v2 reference-key migration, run in the remux data host.

Reuses only a cache entry for the exact currently stored descriptor; never guesses
from a title or copies a fixture into production. No media request is made.
"""
import argparse
import json
from pathlib import Path
import sqlite3
import time
import uuid


def key(source, value):
    return str(uuid.uuid5(source, json.dumps(value, ensure_ascii=False,
                                           sort_keys=True, separators=(',', ':'))))


def migrate(data):
    cache = data / 'subtitle-alignment-reference'
    count = 0
    with sqlite3.connect(f'file:{data / "db.sqlite"}?mode=ro', uri=True) as db:
        for raw_id, raw_info, raw_probe in db.execute(
                'SELECT id,stream_info,probe_data FROM media WHERE stream_info IS NOT NULL'):
            info = json.loads(raw_info)
            if not info or 'Http' not in info.get('descriptor', {}):
                continue
            if not info.get('filename') or not (info.get('size') or 0) > 0:
                continue
            source = uuid.UUID(bytes=raw_id)
            probe = json.loads(raw_probe) if raw_probe else {}
            old = key(source, info['descriptor'])
            new = key(source, {'release': str(source), 'filename': info['filename'],
                              'size': info['size'], 'duration': (probe or {}).get('RunTimeTicks'),
                              'torrent': info.get('torrent_info_hash'),
                              'file_index': info.get('torrent_file_idx'),
                              'usenet_guid': info.get('usenet_guid')})
            for path in cache.glob(old + '_*.srt'):
                if path.stat().st_size > 2_000_000 or time.time()-path.stat().st_mtime > 7*86400:
                    continue
                target = cache / (new + path.name[len(old):])
                if target.exists():
                    continue
                path.rename(target)
                print(f'{path.name} -> {target.name}')
                count += 1
    print(f'Migrated {count} exact-descriptor reference(s); no video bytes fetched.')


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--data-dir', type=Path, required=True)
    migrate(parser.parse_args().data_dir)
