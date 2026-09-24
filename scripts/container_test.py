#!/usr/bin/env python3
"""Check an isolated running deployment before/after container recreation."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import time
from urllib.parse import urlsplit
from urllib.request import Request, urlopen

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('phase', choices=['prepare', 'verify'])
parser.add_argument('state', type=Path)
args = parser.parse_args()
base = os.environ['SPECTRAPLEX_TEST_URL'].rstrip('/')
assert urlsplit(base).hostname in ['127.0.0.1', 'localhost'], 'Use an isolated local deployment'
key = os.environ['SPECTRAPLEX_TEST_KEY']


def call(path, data=None, raw=False):
    request = Request(base + path, data=json.dumps(data).encode() if data is not None else None,
                      headers={'Authorization': f'Bearer {key}', 'Content-Type': 'application/json'})
    with urlopen(request, timeout=15) as response:
        body = response.read()
    return body if raw else json.loads(body)


def completed(path):
    for _ in range(120):
        job = call(path)
        assert job['state'] != 'failed', job
        if job['state'] == 'completed':
            return job
        time.sleep(.5)
    raise AssertionError(f'Timed out: {path}')


assert call('/ready')['version'] == '1.0.0'
assert b'Data workbench' in call('/', raw=True)
if args.phase == 'prepare':
    sample = call('/v1/demo/ethereum', {})
    completed('/v1/jobs/' + sample['materialization_job_id'])
    job = call('/v1/export/dataset', {'dataset': 'raw_transactions', 'target_id': sample['target_id'], 'format': 'jsonl'})
    assert completed('/v1/export/jobs/' + job['id'])['record_count'] == 2
    download = '/v1/export/jobs/' + job['id'] + '/download'
    state = {'target': sample['target_id'], 'download': download,
             'digest': hashlib.sha256(call(download, raw=True)).hexdigest()}
    args.state.parent.mkdir(parents=True, exist_ok=True)
    args.state.touch(mode=0o600, exist_ok=True)
    args.state.write_text(json.dumps(state))
    print('PASS container: migrations, embedded UI, pipeline and export volume writes')
else:
    state = json.loads(args.state.read_text())
    assert call('/v1/targets/' + state['target'])['id'] == state['target']
    assert len(call('/v1/datasets/wallet_ledger/records?target_id=' + state['target'])) == 2
    assert hashlib.sha256(call(state['download'], raw=True)).hexdigest() == state['digest']
    assert call(state['download'] + '?provenance=true')['record_count'] == 2
    print('PASS container: database, jobs, artifacts and provenance survive recreation')
