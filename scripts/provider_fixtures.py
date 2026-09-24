"""Deterministic Ethereum RPC fixture: multi-log transaction and token metadata."""
from copy import deepcopy

ZERO = '0x' + '0' * 64
BLOCK_HASH = '0x' + 'b' * 64
TX_HASH = '0x' + 'e' * 64
RECIPIENT = '0x' + '2' * 40
BLOOM = '0x' + '0' * 512


def evm_response(method, params, wallet, chain_id=1, head=14):
    tx = {'hash': TX_HASH, 'blockHash': BLOCK_HASH, 'blockNumber': '0x1',
          'transactionIndex': '0x0', 'from': wallet, 'to': RECIPIENT, 'value': hex(10**18),
          'gas': '0x5208', 'gasPrice': hex(10**9), 'input': '0x', 'nonce': '0x0',
          'type': '0x0', 'v': '0x1b', 'r': '0x1', 's': '0x1'}
    logs = [{'address': '0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48',
             'topics': ['0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef',
                        '0x' + wallet[2:].rjust(64, '0'), '0x' + RECIPIENT[2:].rjust(64, '0')],
             'data': f'0x{amount * 10**6:064x}', 'logIndex': hex(i), 'removed': False,
             'blockNumber': '0x1', 'blockHash': BLOCK_HASH, 'transactionHash': TX_HASH,
             'transactionIndex': '0x0'} for i, amount in enumerate([10, 20])]
    custom = deepcopy(logs[0])
    custom.update(address='0x' + 'a' * 40, data=f'0x{12 * 10**6:064x}', logIndex='0x2')
    logs.append(custom)
    if method == 'eth_call':
        assert params[0] == {'to': custom['address'], 'data': '0x313ce567'}
        return '0x' + '6'.rjust(64, '0')
    if method == 'eth_chainId':
        return hex(chain_id)
    if method == 'eth_blockNumber':
        return hex(head)
    if method == 'eth_getLogs':
        spec = params[0]
        if int(spec['fromBlock'], 16) <= 1 <= int(spec['toBlock'], 16) and spec['topics'][1]:
            return logs
        return []
    if method == 'eth_getTransactionByHash':
        return tx
    if method == 'eth_getTransactionReceipt':
        return {'transactionHash': TX_HASH, 'transactionIndex': '0x0', 'blockHash': BLOCK_HASH,
                'blockNumber': '0x1', 'from': wallet, 'to': RECIPIENT, 'cumulativeGasUsed': '0x5208',
                'gasUsed': '0x5208', 'effectiveGasPrice': hex(10**9), 'contractAddress': None,
                'logs': logs, 'logsBloom': BLOOM, 'status': '0x1', 'type': '0x0'}
    if method == 'eth_getBlockByNumber':
        number = int(params[0], 16)
        return {'number': hex(number), 'hash': BLOCK_HASH if number == 1 else ZERO,
                'parentHash': ZERO, 'nonce': '0x0000000000000000', 'sha3Uncles': ZERO,
                'logsBloom': BLOOM, 'transactionsRoot': ZERO, 'stateRoot': ZERO,
                'receiptsRoot': ZERO, 'miner': '0x' + '0' * 40, 'difficulty': '0x0',
                'totalDifficulty': '0x0', 'extraData': '0x', 'size': '0x0', 'gasLimit': '0x1c9c380',
                'gasUsed': '0x5208' if number == 1 else '0x0', 'mixHash': ZERO,
                'timestamp': hex(1735689600 + number * 12), 'uncles': [],
                'transactions': ([deepcopy(tx) if params[1] else TX_HASH] if number == 1 else [])}
    raise AssertionError(f'Unexpected EVM RPC method: {method}')
