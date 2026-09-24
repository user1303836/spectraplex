#!/usr/bin/env python3
"""Black-box release checks using an isolated PostgreSQL DB and local RPC fixtures."""
import argparse
import csv
import io
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import tempfile
import threading
import time
from decimal import Decimal
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.error import HTTPError
from urllib.parse import urlsplit, urlunsplit
from urllib.request import Request, urlopen
from provider_fixtures import evm_response

ROOT = Path(__file__).resolve().parent.parent
SAMPLES = json.loads((ROOT / 'api/static/samples.json').read_text())


def check(condition, message):
    if not condition:
        raise AssertionError(message)


def psql(url, sql):
    return subprocess.check_output(['psql', url, '-X', '-v', 'ON_ERROR_STOP=1', '-Atc', sql], text=True).strip()


def wait_for(action, description, timeout=60):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        result = action()
        if result:
            return result
        time.sleep(.2)
    raise AssertionError(f'Timed out: {description}')


class Provider(BaseHTTPRequestHandler):
    errors = []
    evm_chain_id = 1
    evm_head = 14
    evm_blocks = []
    evm_pause = None
    evm_in_request = threading.Event()
    hl_stage = 0

    def log_message(self, *_):
        pass

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        if request.get('type') in ['userFillsByTime', 'userFunding', 'userNonFundingLedgerUpdates']:
            kind = {'userFillsByTime': 'fill', 'userFunding': 'funding', 'userNonFundingLedgerUpdates': 'ledger_update'}[request['type']]
            result = [r['raw_metadata']['data'] for r in SAMPLES['hyperliquid']['records']
                      if r['raw_metadata']['type'] == kind and r['raw_metadata']['data']['time'] >= request['startTime']]
            if self.hl_stage == 0:
                result = [r for r in result if kind == 'ledger_update' or (kind == 'fill' and r['dir'].startswith('Open'))]
            if kind == 'funding':
                result = [{'time': r['time'], 'hash': r.get('hash'), 'delta': {**r, 'type': 'funding'}} for r in result]
        elif request.get('type') == 'fundingHistory':
            result = [{'coin': request['coin'], 'time': 1735689600000 + i * 3600000,
                       'fundingRate': '0.0001', 'premium': '0.00005'} for i in range(2)]
        else:
            method = request.get('method')
            if method and method.startswith('eth_'):
                if method == 'eth_getBlockByNumber':
                    self.evm_blocks.append(int(request['params'][0], 16))
                    pause = self.evm_pause
                    if pause is not None:
                        self.evm_in_request.set()
                        if not pause.wait(30):
                            self.errors.append('Paused fixture was not released')
                result = evm_response(method, request.get('params', []), SAMPLES['ethereum']['wallet'], self.evm_chain_id, self.evm_head)
            elif method == 'getSignaturesForAddress':
                result = [{'signature': signature, 'slot': 300000001, 'err': None,
                           'memo': None, 'blockTime': 1735693200, 'confirmationStatus': 'finalized'}
                          for signature in SIGNATURES]
                if request['params'][1].get('until') or request['params'][1].get('before'):
                    result = []
            elif method == 'getTransaction':
                if request['params'][1].get('maxSupportedTransactionVersion') != 0:
                    self.errors.append('Solana request omitted maxSupportedTransactionVersion=0')
                index = SIGNATURES.index(request['params'][0])
                result = SAMPLES['solana']['records'][index]['raw_metadata']
            else:
                self.errors.append(f'Unexpected provider request: {method}')
                result = None
            result = {'jsonrpc': '2.0', 'id': request.get('id'), 'result': result}
        data = json.dumps(result).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        try:
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError):
            pass  # Expected when the crash/restart test cancels a provider call.


def b58(data):
    alphabet = '123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz'
    number = int.from_bytes(data, 'big')
    value = ''
    while number:
        number, remainder = divmod(number, 58)
        value = alphabet[remainder] + value
    return value


SIGNATURES = [b58(bytes([i]) * 64) for i in [1, 2]]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--browser', action='store_true')
    args = parser.parse_args()
    admin_url = os.environ.get('TEST_DATABASE_URL') or os.environ.get('DATABASE_URL') or 'postgres://spectraplex:spectraplex@localhost:5432/spectraplex'
    db_name = 'spx_smoke_' + secrets.token_hex(6)
    parsed = urlsplit(admin_url)
    db_url = urlunsplit((parsed.scheme, parsed.netloc, '/' + db_name, parsed.query, ''))
    psql(admin_url, f'CREATE DATABASE {db_name}')
    provider = ThreadingHTTPServer(('127.0.0.1', 0), Provider)
    threading.Thread(target=provider.serve_forever, daemon=True).start()
    api_process = None
    with tempfile.TemporaryDirectory(prefix='spectraplex-smoke-') as temp:
        temp = Path(temp)
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            port = sock.getsockname()[1]
        base = f'http://127.0.0.1:{port}'
        admin = secrets.token_hex(32)
        config = temp / 'config.toml'
        provider_url = f'http://127.0.0.1:{provider.server_port}'
        config.write_text(f'''[networks.solana-mainnet]
enabled = true
[networks.hypercore-mainnet]
enabled = true
[networks.ethereum-mainnet]
enabled = true
[[providers]]
network = "ethereum-mainnet"
kind = "rpc"
url = "{provider_url}"
capabilities = ["historical", "logs"]
[[providers]]
network = "solana-mainnet"
kind = "rpc"
url = "{provider_url}"
capabilities = ["historical"]
[[providers]]
network = "hypercore-mainnet"
kind = "rest"
url = "{provider_url}"
capabilities = ["historical"]
''')
        env = {**os.environ, 'DATABASE_URL': db_url, 'SPECTRAPLEX_API_KEY': admin,
               'SPECTRAPLEX_CONFIG': str(config), 'SPECTRAPLEX_HOST': '127.0.0.1',
               'SPECTRAPLEX_PORT': str(port), 'SPECTRAPLEX_INGEST_LIMIT': '50',
               'EXPORT_DIR': str(temp / 'exports'), 'SPECTRAPLEX_ENABLE_V1_COMPAT_WRITES': 'false'}
        log = (temp / 'api.log').open('w+')

        def call(path, key=admin, data=None, method=None, status=200, raw=False):
            request = Request(base + path, data=json.dumps(data).encode() if data is not None else None,
                              headers={**({'Authorization': f'Bearer {key}'} if key else {}), 'Content-Type': 'application/json'},
                              method=method or ('POST' if data is not None else 'GET'))
            try:
                response = urlopen(request, timeout=15)
            except HTTPError as error:
                response = error
            body = response.read()
            check(response.status == status, f'{path}: expected {status}, got {response.status}: {body[:1000]!r}')
            if raw:
                return body
            return json.loads(body) if body else None

        def start():
            nonlocal api_process
            api_process = subprocess.Popen([str(ROOT / 'target/debug/spectraplex-api')], cwd=temp, env=env, stdout=log, stderr=log)
            def ready():
                if api_process.poll() is not None:
                    raise AssertionError('API exited during startup')
                try:
                    return call('/ready', key=None)['status'] == 'ready'
                except OSError:
                    return False
            wait_for(ready, 'API readiness')

        def stop():
            if api_process and api_process.poll() is None:
                api_process.terminate()
                try:
                    api_process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    api_process.kill()
                    api_process.wait(timeout=5)
                    raise AssertionError('API failed graceful SIGTERM shutdown')

        def job_done(job_id, key, export=False):
            path = f'/v1/export/jobs/{job_id}' if export else f'/v1/jobs/{job_id}'
            def poll():
                result = call(path, key)
                check(result['state'] != 'failed', f'Job failed: {result}')
                return result if result['state'] == 'completed' else False
            return wait_for(poll, path)

        def records(dataset, target, key):
            return call(f'/v1/datasets/{dataset}/records?target_id={target}&limit=1000', key)

        def export(dataset, target, key, fmt='jsonl'):
            job = call('/v1/export/dataset', key, {'dataset': dataset, 'target_id': target, 'format': fmt}, status=202)
            result = job_done(job['id'], key, True)
            data = call(f"/v1/export/jobs/{job['id']}/download", key, raw=True)
            provenance = call(f"/v1/export/jobs/{job['id']}/download?provenance=true", key)
            check(provenance['record_count'] == result['record_count'], 'Provenance count mismatch')
            parsed_data = [json.loads(line) for line in data.splitlines()] if fmt == 'jsonl' else list(csv.DictReader(io.StringIO(data.decode())))
            check(len(parsed_data) == result['record_count'], 'Artifact row count mismatch')
            return job, parsed_data

        try:
            start()
            check(call('/ready')['version'] == '1.0.0', 'Wrong release version')
            check(b'Data workbench' in call('/', key=None, raw=True), 'Workbench missing')
            for path in ['/v1/session', '/v1/jobs', '/v1/targets']:
                call(path, key=None, status=401)
            tenant = call('/v1/api-keys', data={'name': 'Smoke tenant'}, status=201)
            tenant2 = call('/v1/api-keys', data={'name': 'Other tenant'}, status=201)
            key, other = tenant['key'], tenant2['key']
            targets = {}
            for chain, sample in SAMPLES.items():
                imported = call(f'/v1/demo/{chain}', key, {}, status=200)
                targets[chain] = imported['target_id']
                job_done(imported['materialization_job_id'], key)
                check(len(records('raw_transactions', imported['target_id'], key)) == len(sample['records']), f'{chain} raw rows missing')
                ledger = records('wallet_ledger', imported['target_id'], key)
                check(len(ledger) >= 2, f'{chain} ledger is empty or incomplete: {ledger}')
                export('wallet_ledger', imported['target_id'], key, 'csv')
                export('raw_transactions', imported['target_id'], key)
            sol_ledger = records('wallet_ledger', targets['solana'], key)
            check(sum(Decimal(str(r['amount'])) for r in sol_ledger if r['entry_type'] == 'fee') == Decimal('-0.000005'), 'Solana transfer was misclassified as fee')
            hl_ledger = records('wallet_ledger', targets['hyperliquid'], key)
            check(sum(Decimal(str(r['amount'])) for r in hl_ledger if r['asset_symbol'] == 'USDC') == Decimal('1100.45'), 'HL settlement was duplicated or fees/PnL omitted')
            check(all(r['timestamp'] < 2_000_000_000 for r in hl_ledger), 'Gold timestamps must use seconds')
            pnl = records('hl_pnl_summary', targets['hyperliquid'], key)
            check(len(pnl) == 1 and Decimal(str(pnl[0]['net_pnl'])) == Decimal('100.45'), 'HL net PnL mismatch')
            print('PASS samples → Bronze → Silver/Gold → CSV/JSONL + provenance, with exact financial fixtures')

            sol = targets['solana']
            balance_before = records('balance_history', sol, key)
            replay = call('/v1/demo/solana', key, {})
            job_done(replay['materialization_job_id'], key)
            balance_after = records('balance_history', sol, key)
            stable = lambda rows: sorted((r['asset_symbol'], r['timestamp'], str(r['balance'])) for r in rows)
            check(stable(balance_before) == stable(balance_after), 'Replay changed balances')
            check(sum(Decimal(str(r['amount'])) for r in records('wallet_ledger', targets['ethereum'], key)) == Decimal('175'), 'ERC20 ledger expected +250 -75 USDC')
            for chain in ['ethereum', 'hyperliquid']:
                before = stable(records('balance_history', targets[chain], key))
                again = call(f'/v1/demo/{chain}', key, {})
                job_done(again['materialization_job_id'], key)
                check(stable(records('balance_history', targets[chain], key)) == before, f'{chain} replay changed balances')
            print('PASS three-chain replay idempotency and representative numeric results')

            for path in [f'/v1/targets/{sol}', f'/v1/datasets/wallet_ledger/records?target_id={sol}']:
                call(path, other, status=403)
            check(call('/v1/jobs', other) == [], 'Jobs leaked to another tenant')
            check(call('/v1/targets', other) == [], 'Targets leaked to another tenant')
            call(f'/v1/targets/{sol}/import', key, {'records': SAMPLES['solana']['records']}, status=403)
            second = call('/v1/demo/solana', other, {})
            job_done(second['materialization_job_id'], other)
            # Replaying a previous run must still see its immutable inputs after another target ingests them.
            renormalize = call('/v1/normalize', key, {'wallet': SAMPLES['solana']['wallet'], 'ingestion_run_id': replay['ingestion_run_id']})
            job_done(renormalize['id'], key)
            check(stable(records('balance_history', sol, key)) == stable(balance_before), 'Shared transaction changed original history')
            print('PASS tenant boundaries and immutable run inputs')

            registered = call('/v1/targets', data={'kind': 'wallet', 'network': 'ethereum-mainnet', 'address': SAMPLES['ethereum']['wallet'], 'mode': 'backfill'}, status=201)
            call(f"/v1/targets/{registered['id']}/import", data={'records': []}, status=400)
            call(f"/v1/targets/{registered['id']}/import", data={'records': [{'tx_hash': 'invalid', 'timestamp': 1, 'raw_metadata': {}}]}, status=400)
            malformed = json.loads(json.dumps(SAMPLES['ethereum']['records'][0]))
            malformed['raw_metadata']['data'] = '0xNOTHEX'
            call(f"/v1/targets/{registered['id']}/import", data={'records': [malformed]}, status=400)
            malformed['raw_metadata'] = {'logs': [None]}
            call(f"/v1/targets/{registered['id']}/import", data={'records': [malformed]}, status=400)
            imported = call(f"/v1/targets/{registered['id']}/import", data={'records': SAMPLES['ethereum']['records']})
            job_done(imported['materialization_job_id'], admin)
            check(len(records('token_transfers', registered['id'], admin)) == 2, 'Admin import did not materialize')
            older = json.loads(json.dumps(SAMPLES['ethereum']['records'][0]))
            older['tx_hash'] = '0x' + '9' * 64
            older['timestamp'] -= 86400
            older['raw_metadata']['data'] = f'0x{50 * 1000000:064x}'
            backfill = call(f"/v1/targets/{registered['id']}/import", data={'records': [older]})
            job_done(backfill['materialization_job_id'], admin)
            balances = records('balance_history', registered['id'], admin)
            check(len(balances) == 3 and Decimal(str(max(balances, key=lambda r: r['timestamp'])['balance'])) == Decimal('225'), 'Older arrival did not recompute balance suffix')
            self_transfer = json.loads(json.dumps(older))
            self_transfer['tx_hash'] = '0x' + '8' * 64
            self_transfer['timestamp'] += 200000
            self_transfer['raw_metadata']['topics'][1] = self_transfer['raw_metadata']['topics'][2]
            self_job = call(f"/v1/targets/{registered['id']}/import", data={'records': [self_transfer]})
            job_done(self_job['materialization_job_id'], admin)
            check(len(records('wallet_ledger', registered['id'], admin)) == 3, 'Self-transfer created spurious ledger income/expense')
            unknown = json.loads(json.dumps(older))
            unknown['tx_hash'] = '0x' + '6' * 64
            unknown['raw_metadata']['address'] = '0x' + 'a' * 40
            unknown_job = call(f"/v1/targets/{registered['id']}/import", data={'records': [unknown]})
            job_done(unknown_job['materialization_job_id'], admin)
            unknown_rows = [r for r in records('token_transfers', registered['id'], admin) if r['token_address'] == unknown['raw_metadata']['address']]
            check(len(unknown_rows) == 1 and unknown_rows[0]['decimals'] == -1 and Decimal(str(unknown_rows[0]['amount'])) == Decimal('50000000'), 'Unknown ERC20 decimals were guessed')
            check(len(records('wallet_ledger', registered['id'], admin)) == 3, 'Unknown units entered financial totals')
            # Bronze is immutable: this is a second observed transaction, supplied with metadata.
            unknown['tx_hash'] = '0x' + '5' * 64
            unknown['raw_metadata']['token_decimals'] = {unknown['raw_metadata']['address']: 6}
            known_job = call(f"/v1/targets/{registered['id']}/import", data={'records': [unknown]})
            job_done(known_job['materialization_job_id'], admin)
            known_rows = [r for r in records('wallet_ledger', registered['id'], admin) if r['asset_symbol'] == unknown['raw_metadata']['address']]
            check(len(known_rows) == 1 and Decimal(str(known_rows[0]['amount'])) == 50, 'Explicit token decimals were ignored')
            print('PASS validated import, unknown-decimal safety, self-transfer neutrality and out-of-order balances')

            call('/v1/targets', key, {'kind': 'wallet', 'network': 'ethereum-mainnet', 'address': SAMPLES['ethereum']['wallet'], 'filter_spec': {'from_block': 1}}, status=201)
            evm = call('/v1/ingest', key, {'wallet': SAMPLES['ethereum']['wallet'], 'network': 'ethereum-mainnet'})
            job_done(evm['id'], key)
            check(min(Provider.evm_blocks) == 1, 'EVM ignored requested starting block')
            evm_target = next(t['id'] for t in call('/v1/targets', key) if t['network'] == 'ethereum-mainnet')
            def evm_materialized():
                jobs = call(f'/v1/jobs?target_id={evm_target}', key)
                failed = [j for j in jobs if j['state'] == 'failed']
                check(not failed, f'EVM materialization failed: {failed}')
                return any(j['kind'] == 'materialize' and j['state'] == 'completed' for j in jobs)
            wait_for(evm_materialized, 'EVM materialization')
            eraw = records('raw_transactions', evm_target, key)
            check(len(eraw) == 1 and len(eraw[0]['raw_metadata']['logs']) == 3 and eraw[0]['timestamp'] == 1735689612, 'EVM lost logs or timestamp')
            transfers = records('token_transfers', evm_target, key)
            check(len(transfers) == 3 and sum(Decimal(str(r['amount'])) for r in transfers if r['token_symbol'] == 'USDC') == 30, 'Multi-log EVM transfer loss')
            custom = next(r for r in transfers if r['token_address'] == '0x' + 'a' * 40)
            check(custom['decimals'] == 6 and Decimal(str(custom['amount'])) == 12, 'Provider token metadata was not applied')
            eledger = records('wallet_ledger', evm_target, key)
            # Wallet-scoped Gold includes other indexed records for this public wallet.
            native = [r for r in eledger if r['asset_symbol'] == 'ETH']
            check(sum(Decimal(str(r['amount'])) for r in native) == Decimal('-1.000021'), 'EVM value or fee incorrect')
            Provider.evm_head += 2
            Provider.evm_blocks.clear()
            empty = call(f'/v1/targets/{evm_target}/ingest', key, {})
            job_done(empty['id'], key)
            check(min(Provider.evm_blocks) == 3, 'EVM rescanned the initial range instead of resuming its checkpoint')
            checkpoint = psql(db_url, f"SELECT cursor->>'last_block' FROM checkpoints WHERE target_id='{evm_target}' AND source='rpc'")
            check(checkpoint == '4', f'Empty EVM window did not advance checkpoint: {checkpoint}')
            Provider.evm_chain_id = 2
            mismatch = call(f'/v1/targets/{evm_target}/ingest', key, {'mode': 'backfill'})
            rejected = wait_for(lambda: (r if (r := call(f"/v1/jobs/{mismatch['id']}", key))['state'] == 'failed' else False), 'EVM chain mismatch rejection')
            check('chain ID mismatch' in rejected['message'], f'Unexpected EVM failure: {rejected}')
            check(psql(db_url, f"SELECT cursor->>'last_block' FROM checkpoints WHERE target_id='{evm_target}' AND source='rpc'") == '4', 'Failed ingestion advanced checkpoint')
            Provider.evm_chain_id = 1
            print('PASS EVM multi-log/value/gas ingestion, bounded windows, empty checkpoints and wrong-chain rejection')

            hl_first = call('/v1/ingest', key, {'wallet': SAMPLES['hyperliquid']['wallet'], 'network': 'hypercore-mainnet'})
            job_done(hl_first['id'], key)
            hl_target = next(t['id'] for t in call('/v1/targets', key) if t['network'] == 'hypercore-mainnet')
            def hl_materializations(count):
                jobs = [j for j in call(f'/v1/jobs?target_id={hl_target}', key) if j['kind'] == 'materialize']
                check(not any(j['state'] == 'failed' for j in jobs), f'HL materialization failed: {jobs}')
                return jobs if len(jobs) >= count and all(j['state'] == 'completed' for j in jobs) else False
            wait_for(lambda: hl_materializations(1), 'first HL batch')
            Provider.hl_stage = 1
            hl_second = call(f'/v1/targets/{hl_target}/ingest', key, {})
            job_done(hl_second['id'], key)
            hl_jobs = wait_for(lambda: hl_materializations(2), 'second HL batch')
            hl_summary = records('hl_pnl_summary', hl_target, key)
            check(len(hl_summary) == 1 and Decimal(str(hl_summary[0]['net_pnl'])) == Decimal('100.45') and hl_summary[0]['fill_count'] == 2, 'HL analytics depended on ingestion batch boundaries')
            hl_trades = records('hl_trade_history', hl_target, key)
            check(len(hl_trades) == 1 and hl_trades[0]['num_fills'] == 2 and hl_trades[0]['closed_at'] < 2_000_000_000, 'HL trade missed its earlier opening fill or has wrong timestamp units')
            replay_hl = call('/v1/normalize', key, {'wallet': SAMPLES['hyperliquid']['wallet'], 'ingestion_run_id': hl_jobs[0]['ingestion_run_id']})
            job_done(replay_hl['id'], key)
            check([r['id'] for r in records('hl_trade_history', hl_target, key)] == [r['id'] for r in hl_trades], 'HL replay forked trades')
            check(len(records('hl_pnl_summary', hl_target, key)) == 1, 'HL replay duplicated PnL')
            print('PASS HL nested funding, independent cursors, multi-batch analytics and stable replay')

            live = call('/v1/ingest', key, {'wallet': SAMPLES['solana']['wallet'], 'network': 'solana-mainnet'})
            job_done(live['id'], key)
            market = call('/v1/targets', key, {'kind': 'market', 'network': 'hypercore-mainnet', 'address': 'ETH', 'mode': 'backfill'}, status=201)
            fetch = call(f"/v1/targets/{market['id']}/ingest", key, {'mode': 'backfill'})
            job_done(fetch['id'], key)
            check(len(records('raw_transactions', market['id'], key)) == 2, 'Non-wallet provider path returned no raw records')
            export('raw_transactions', market['id'], key, 'csv')
            check(not Provider.errors, str(Provider.errors))
            print('PASS local-provider Solana v0 and non-wallet market ingestion')

            # Stop during a real provider request, not merely after completed work.
            Provider.evm_head = 18
            Provider.evm_pause = threading.Event()
            recovering = call(f'/v1/targets/{evm_target}/ingest', key, {})
            check(Provider.evm_in_request.wait(10), 'Ingestion never reached paused provider')
            check(call(f"/v1/jobs/{recovering['id']}", key)['state'] == 'running', 'Expected a genuinely running ingestion')
            pending = call('/v1/export/dataset', key, {'dataset': 'wallet_ledger', 'target_id': sol, 'format': 'jsonl'}, status=202)
            stop()
            Provider.evm_pause.set()
            Provider.evm_pause = None
            check(psql(db_url, f"SELECT cursor->>'last_block' FROM checkpoints WHERE target_id='{evm_target}' AND source='rpc'") == '4', 'Shutdown committed unfinished ingestion')
            # Advance lease age deterministically instead of sleeping five minutes.
            psql(db_url, f"UPDATE ingestion_job_attempts SET heartbeat_at=NOW()-INTERVAL '1 hour' WHERE job_id='{recovering['id']}'")
            start()
            job_done(recovering['id'], key)
            check(psql(db_url, f"SELECT MAX(attempt_num) FROM ingestion_job_attempts WHERE job_id='{recovering['id']}'") == '2', 'Restart did not reclaim abandoned work')
            check(psql(db_url, f"SELECT cursor->>'last_block' FROM checkpoints WHERE target_id='{evm_target}' AND source='rpc'") == '6', 'Recovered ingestion did not resume from its durable checkpoint')
            job_done(pending['id'], key, True)
            check(call(f"/v1/export/jobs/{pending['id']}/download", key, raw=True), 'Restart lost export')
            call(f"/v1/export/jobs/{pending['id']}/download", other, status=403)
            check(stable(records('balance_history', sol, key)) == stable(balance_before), 'Restart changed data')
            check(not Provider.errors, str(Provider.errors))
            print('PASS in-flight restart/lease recovery, pending export and durable downloads')

            if args.browser:
                subprocess.run(['node', str(ROOT / 'web-tests/workbench.mjs')], env={**os.environ, 'SPECTRAPLEX_TEST_URL': base, 'SPECTRAPLEX_TEST_KEY': admin}, check=True)
            call(f"/v1/api-keys/{tenant2['id']}", other, method='DELETE', status=204)
            call('/v1/session', other, status=401)
            print('PASS key revocation\nAll end-to-end checks passed.')
        except Exception:
            log.flush()
            print('\nAPI log (last 60 lines):')
            print('\n'.join((temp / 'api.log').read_text().splitlines()[-60:]))
            raise
        finally:
            stop()
            log.close()
            provider.shutdown()
            psql(admin_url, f'DROP DATABASE {db_name} WITH (FORCE)')


if __name__ == '__main__':
    main()
