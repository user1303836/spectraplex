'use strict';
const $ = id => document.getElementById(id);
const state = { key: '', session: null, targets: [], networks: [], datasets: [], target: null, offset: 0, jobOffset: 0, query: 0, jobs: [], pollBusy: false };
const PAGE_SIZE = 50;
const busy = new WeakSet();
let requests = new AbortController();
const node = (tag, text, className) => {
  const el = document.createElement(tag);
  if (text !== undefined) el.textContent = text;
  if (className) el.className = className;
  return el;
};
function notify(message, error = false) {
  $('notice').textContent = message;
  $('notice').className = `notice${error ? ' error' : ''}`;
  $('notice').setAttribute('role', error ? 'alert' : 'status');
  $('notice').hidden = false;
}
async function api(path, options = {}) {
  const response = await fetch(path, { ...options, signal: requests.signal, headers: { Authorization: `Bearer ${state.key}`, ...(options.body ? { 'Content-Type': 'application/json' } : {}) }, cache: 'no-store' });
  if (!response.ok) {
    const text = await response.text();
    let message;
    try { message = JSON.parse(text).error; } catch { message = text; }
    if (response.status === 401 && state.session) disconnect();
    throw new Error(message || `Request failed (${response.status})`);
  }
  return options.download ? response.blob() : response.status === 204 ? null : response.json();
}
const post = (path, body) => api(path, { method: 'POST', body: JSON.stringify(body) });
function run(element, action) {
  return async event => {
    event?.preventDefault();
    const button = element.tagName === 'FORM' ? element.querySelector('button[type="submit"], button:not([type])') : element;
    if (button) { button.disabled = true; busy.add(button); }
    try { await action(event); }
    catch (error) { if (error.name !== 'AbortError') notify(error.message || 'Cannot reach the API. Check that the server is running.', true); }
    finally { if (button) { busy.delete(button); button.disabled = false; } syncActions(); }
  };
}
function disconnect() {
  requests.abort(); requests = new AbortController();
  state.key = ''; state.session = null; state.target = null; state.targets = []; state.jobs = []; state.query++;
  $('workspace').hidden = true; $('login').hidden = false; $('disconnect').hidden = true;
  $('api-key').value = ''; $('new-key').value = ''; $('new-key-box').hidden = true;
  $('connection-state').textContent = 'Self-hosted data workbench';
  for (const id of ['target-list', 'keys-table', 'jobs-table', 'records-table']) {
    const el = $(id); const body = el.querySelector('tbody'); if (body) body.replaceChildren(); else el.replaceChildren();
  }
  $('api-key').focus();
}
function panel(name) {
  for (const button of document.querySelectorAll('[data-panel]')) {
    const selected = button.dataset.panel === name;
    if (selected) button.setAttribute('aria-current', 'page'); else button.removeAttribute('aria-current');
    $(`panel-${button.dataset.panel}`).hidden = !selected;
  }
}
function targetOptions() {
  const search = $('target-search').value.toLowerCase();
  $('target-list').replaceChildren();
  for (const target of state.targets.filter(t => `${t.label || ''} ${t.address || ''} ${t.network}`.toLowerCase().includes(search))) {
    const option = node('option', target.label || `${target.network} · ${(target.address || '').slice(0, 14)}…`);
    option.value = target.id; option.selected = target.id === state.target?.id;
    option.title = `${target.network} · ${target.address || ''}`;
    $('target-list').append(option);
  }
  if (!state.target) $('target-list').selectedIndex = -1;
}
async function loadTargets(selectId) {
  // Follow the API's bounded pages rather than silently hiding targets beyond the first page.
  const targets = [];
  for (let offset = 0; ; offset += 1000) {
    const page = await api(`/v1/targets?limit=1000&offset=${offset}`);
    targets.push(...page); if (page.length < 1000) break;
  }
  state.targets = targets;
  const selected = selectId || state.target?.id || targets[0]?.id;
  targetOptions();
  await selectTarget(selected || null);
}
function networkOptions() {
  const previous = $('target-network').value;
  $('target-network').replaceChildren();
  const market = $('target-kind').value === 'market';
  for (const network of state.networks.filter(n => !n.id.endsWith('-demo') && (!market || n.chain_family === 'hyperliquid'))) {
    const configured = state.session.configured_networks.includes(network.id);
    const option = node('option', `${network.display_name}${configured ? '' : ' (configure provider / import)'}`);
    option.value = network.id; $('target-network').append(option);
  }
  if ([...$('target-network').options].some(o => o.value === previous)) $('target-network').value = previous;
  startBlockField();
}
function startBlockField() {
  $('start-block-field').hidden = state.networks.find(n => n.id === $('target-network').value)?.chain_family !== 'evm';
}
function syncActions() {
  const target = state.target;
  $('ingest').disabled = busy.has($('ingest')) || !target || !state.session?.configured_networks.includes(target.network) || target.network.endsWith('-demo');
  for (const [id, disabled] of [['previous', state.offset === 0], ['next', state.recordsCount !== PAGE_SIZE], ['jobs-previous', state.jobOffset === 0], ['jobs-next', state.jobsCount !== PAGE_SIZE]]) {
    $(id).disabled = busy.has($(id)) || disabled;
  }
  $('backfill').hidden = !target || target.chain_family !== 'solana' || target.network.endsWith('-demo');
  $('backfill').disabled = busy.has($('backfill')) || $('ingest').disabled;
  $('import-section').hidden = !state.session?.admin || !target || target.network.endsWith('-demo');
}
async function selectTarget(id) {
  state.target = state.targets.find(t => t.id === id) || null;
  state.offset = 0;
  targetOptions(); syncActions();
  $('empty-target').hidden = !!state.target; $('target-view').hidden = !state.target;
  if (!state.target) return;
  const target = state.target;
  $('selected-target-name').textContent = target.label || 'Untitled target';
  $('selected-target-address').textContent = target.address || target.id;
  $('target-network-label').textContent = target.network;
  const sample = target.network.endsWith('-demo');
  $('target-caveat').textContent = sample ? 'SYNTHETIC SAMPLE · Not live chain data. Reloading this sample is safe; records are deduplicated.' : target.kind === 'market' ? 'Market targets capture raw funding-rate history. Select Raw transactions to inspect or export it.' : target.chain_family === 'evm' ? 'EVM scans forward in bounded block windows, starting at your chosen block or recent blocks—not full wallet history. The wallet ledger excludes internal transfers and fees beyond execution gas.' : 'Fetch is bounded by your configured ingest limit. It is not a guarantee of full wallet history.';
  const previous = $('dataset').value;
  $('dataset').replaceChildren();
  for (const ds of state.datasets.filter(d => d.queryable && d.chain_families.includes(target.chain_family) && (target.kind === 'wallet' || d.tier === 'bronze'))) {
    const option = node('option', `${ds.name.replaceAll('_', ' ')} · ${ds.tier}`); option.value = ds.name; $('dataset').append(option);
  }
  const names = [...$('dataset').options].map(o => o.value);
  $('dataset').value = names.includes(previous) ? previous : target.kind === 'wallet' ? 'wallet_ledger' : 'raw_transactions';
  await loadRecords();
}
function filters() {
  const params = new URLSearchParams({ target_id: state.target.id, network: state.target.network });
  const from = $('from').value ? Math.floor(new Date($('from').value).getTime() / 1000) : null;
  const to = $('to').value ? Math.floor(new Date($('to').value).getTime() / 1000) : null;
  if ((from !== null && !Number.isFinite(from)) || (to !== null && !Number.isFinite(to))) throw new Error('Enter valid dates.');
  if (from !== null && to !== null && from > to) throw new Error('From must be earlier than To.');
  if (from !== null) params.set('time_start', from);
  if (to !== null) params.set('time_end', to);
  return params;
}
const columns = {
  raw_transactions: ['timestamp', 'tx_hash', 'block_number', 'source'],
  wallet_ledger: ['timestamp', 'entry_type', 'asset_symbol', 'amount', 'counterparty_address', 'tx_hash'],
  balance_history: ['timestamp', 'asset_symbol', 'balance', 'delta'],
  token_transfers: ['token_symbol', 'from_address', 'to_address', 'amount', 'decimals'],
  native_balance_deltas: ['account_address', 'native_token', 'pre_balance', 'post_balance', 'delta'],
  decoded_events: ['event_name', 'program_or_contract', 'event_index'],
  hl_fills: ['coin', 'side', 'price', 'size', 'closed_pnl', 'fee'],
  hl_funding: ['coin', 'usdc', 'funding_rate'],
  hl_pnl_summary: ['coin', 'total_closed_pnl', 'total_funding', 'total_fees', 'net_pnl'],
  hl_trade_history: ['coin', 'side', 'entry_price', 'exit_price', 'size', 'realized_pnl'],
};
function valueText(value, key) {
  if (value === null || value === undefined) return '—';
  if (key === 'timestamp' && typeof value === 'number') return new Date(value * 1000).toLocaleString();
  if (typeof value === 'object') return JSON.stringify(value);
  return String(value);
}
function renderRecords(records, dataset) {
  const head = $('records-table').querySelector('thead'); const body = $('records-table').querySelector('tbody');
  head.replaceChildren(); body.replaceChildren();
  const preferred = columns[dataset] || [];
  const fields = records[0] ? (preferred.filter(c => c in records[0]).length ? preferred.filter(c => c in records[0]) : Object.keys(records[0]).filter(c => !['id', 'dataset_version_id', 'raw_transaction_id', 'created_at'].includes(c)).slice(0, 6)) : preferred;
  const row = node('tr'); for (const field of [...fields, 'Details']) row.append(node('th', field.replaceAll('_', ' '))); head.append(row);
  for (const record of records) {
    const tr = node('tr');
    for (const field of fields) {
      const value = valueText(record[field], field);
      const cell = node('td', value, /amount|balance|price|size|pnl|fee|delta/.test(field) ? 'numeric' : '');
      const identifier = /tx_hash|address|program_or_contract/.test(field) && value.length > 28;
      if (identifier) cell.classList.add('identifier');
      if (identifier || value.length > 90) { cell.textContent = `${value.slice(0, identifier ? 12 : 36)}…${value.slice(identifier ? -8 : -12)}`; cell.title = value; }
      tr.append(cell);
    }
    const cell = node('td'); const details = node('details'); details.append(node('summary', 'View JSON'), node('pre', JSON.stringify(record, null, 2))); cell.append(details); tr.append(cell); body.append(tr);
  }
  $('empty-records').hidden = records.length !== 0;
}
async function loadRecords() {
  if (!state.target || !state.key) return;
  const query = ++state.query; const dataset = $('dataset').value; const targetId = state.target.id;
  const params = filters(); params.set('limit', PAGE_SIZE); params.set('offset', state.offset);
  $('record-count').textContent = 'Loading…';
  renderRecords([], dataset); $('empty-records').hidden = true;
  let records;
  try { records = await api(`/v1/datasets/${dataset}/records?${params}`); }
  catch (error) { if (query === state.query) $('record-count').textContent = 'Records unavailable'; throw error; }
  if (query !== state.query) return;
  state.recordsCount = records.length;
  renderRecords(records, dataset);
  $('record-count').textContent = `${records.length} records on this page`;
  $('previous').disabled = state.offset === 0; $('next').disabled = records.length < PAGE_SIZE;
  $('page-label').textContent = `Page ${state.offset / PAGE_SIZE + 1}`;
  const completeness = await api(`/v1/datasets/${dataset}/completeness`);
  if (query !== state.query) return;
  const coverage = completeness.find(c => c.target_id === targetId);
  const status = coverage?.status || 'unknown';
  $('coverage').textContent = status === 'complete' ? 'Indexed batch processed' : status === 'unknown' ? 'Coverage unknown' : `Coverage: ${status}`;
  $('coverage').className = `badge ${['complete', 'partial', 'gap', 'backfilling'].includes(status) ? status : ''}`;
  $('coverage').title = coverage?.notes || 'Coverage refers to indexed data, not proof of complete chain history.';
}
async function download(job, provenance = false) {
  const blob = await api(`/v1/export/jobs/${job.id}/download${provenance ? '?provenance=true' : ''}`, { download: true });
  const url = URL.createObjectURL(blob); const link = node('a');
  link.href = url; link.download = `${job.dataset}-${job.id}.${provenance ? 'provenance.json' : job.format}`;
  document.body.append(link); link.click(); link.remove(); setTimeout(() => URL.revokeObjectURL(url), 30000);
}
async function loadJobs() {
  if (!state.key) return;
  const jobs = await api(`/v1/jobs?limit=${PAGE_SIZE}&offset=${state.jobOffset}`);
  const previous = state.jobs;
  state.jobs = jobs;
  state.jobsCount = jobs.length;
  const active = jobs.filter(j => ['pending', 'running', 'claimed', 'delivering'].includes(j.state));
  $('pending-count').textContent = active.length || '';
  const body = $('jobs-table').querySelector('tbody'); body.replaceChildren();
  for (const job of jobs) {
    const tr = node('tr'); const operation = node('td', job.kind); operation.title = job.id; tr.append(operation);
    tr.append(node('td', job.kind === 'export' ? `${job.dataset} · ${job.format}` : job.network || job.dataset));
    const status = node('td'); status.append(node('span', job.state, `badge ${job.state}`)); tr.append(status);
    tr.append(node('td', new Date(job.created_at).toLocaleString()));
    const result = node('td');
    if (job.kind === 'export' && job.state === 'completed') {
      for (const [label, provenance] of [['Download', false], ['Provenance', true]]) {
        const button = node('button', label); button.addEventListener('click', run(button, () => download(job, provenance))); result.append(button);
      }
      result.append(node('div', `${job.record_count ?? 0} records`, 'hint'));
    } else result.textContent = job.message || (job.state === 'completed' ? `${job.record_count ?? ''}${job.record_count !== null ? ' records' : ''}` : '');
    if (job.state === 'failed' && ['materialize', 'ingest'].includes(job.kind)) {
      const retry = node('button', 'Retry');
      retry.addEventListener('click', run(retry, async () => {
        if (job.kind === 'materialize') await post('/v1/normalize', { wallet: job.wallet, ingestion_run_id: job.ingestion_run_id });
        else await post(`/v1/targets/${job.target_id}/ingest`, { mode: job.mode || 'incremental' });
        notify('Retry queued.'); await loadJobs();
      }));
      result.append(node('br'), retry);
    }
    tr.append(result); body.append(tr);
  }
  $('empty-jobs').hidden = jobs.length !== 0;
  $('jobs-previous').disabled = state.jobOffset === 0; $('jobs-next').disabled = jobs.length < PAGE_SIZE;
  $('jobs-page').textContent = `Page ${state.jobOffset / PAGE_SIZE + 1}`;
  const justFinished = jobs.some(j => j.kind === 'materialize' && j.state === 'completed' && !previous.some(p => p.id === j.id && p.state === 'completed'));
  if (justFinished && state.target && !$('panel-explore').hidden) await loadRecords();
}
async function loadKeys() {
  const keys = await api('/v1/api-keys'); const body = $('keys-table').querySelector('tbody'); body.replaceChildren();
  for (const key of keys) {
    const tr = node('tr'); tr.append(node('td', key.name), node('td', key.id.slice(0, 8)), node('td', new Date(key.created_at).toLocaleString()));
    const td = node('td'); const button = node('button', 'Revoke');
    button.addEventListener('click', run(button, async () => {
      if (!confirm(`Revoke “${key.name}”? Clients using this key will lose access.`)) return;
      await api(`/v1/api-keys/${key.id}`, { method: 'DELETE' });
      notify('Key revoked.'); await loadKeys();
    })); td.append(button); tr.append(td); body.append(tr);
  }
}
$('login-form').addEventListener('submit', run($('login-form'), async () => {
  state.key = $('api-key').value.trim();
  if (!state.key) throw new Error('Enter your API key.');
  state.session = await api('/v1/session');
  [state.networks, state.datasets] = await Promise.all([api('/v1/networks'), api('/v1/datasets')]);
  $('api-key').value = ''; $('login').hidden = true; $('workspace').hidden = false; $('disconnect').hidden = false;
  $('connection-state').textContent = `${state.session.admin ? 'Admin' : 'Tenant workspace'} · v${state.session.version}`;
  $('key-explanation').textContent = state.session.admin ? 'Each new key creates an isolated tenant workspace. Your admin key can access all data; share tenant keys instead.' : 'New keys share your current workspace. Revoking your current key disconnects this session.';
  $('notice').hidden = true; state.offset = 0; state.jobOffset = 0; panel('explore'); networkOptions();
  await loadTargets(); await loadJobs();
}));
$('disconnect').addEventListener('click', disconnect);
for (const button of document.querySelectorAll('[data-panel]')) button.addEventListener('click', run(button, async () => { panel(button.dataset.panel); if (button.dataset.panel === 'activity') await loadJobs(); if (button.dataset.panel === 'access') await loadKeys(); }));
$('target-search').addEventListener('input', targetOptions);
$('target-list').addEventListener('change', run($('target-list'), () => selectTarget($('target-list').value)));
$('target-kind').addEventListener('change', networkOptions);
$('target-network').addEventListener('change', startBlockField);
$('refresh-targets').addEventListener('click', run($('refresh-targets'), () => loadTargets()));
$('target-form').addEventListener('submit', run($('target-form'), async () => {
  const block = $('target-start-block').value;
  const filter_spec = !$('start-block-field').hidden && block !== '' ? { from_block: Number(block) } : null;
  if (filter_spec && (!Number.isSafeInteger(filter_spec.from_block) || filter_spec.from_block < 0)) throw new Error('Starting block must be a nonnegative integer.');
  const target = await post('/v1/targets', { kind: $('target-kind').value, network: $('target-network').value, address: $('target-address').value.trim(), label: $('target-label').value.trim() || null, mode: 'backfill', filter_spec });
  $('target-address').value = ''; $('target-label').value = ''; $('target-start-block').value = ''; $('add-target').open = false;
  notify('Target created. Fetch activity, or import trusted data with an admin key.'); await loadTargets(target.id);
}));
for (const button of document.querySelectorAll('[data-demo]')) button.addEventListener('click', run(button, async () => {
  const result = await post(`/v1/demo/${button.dataset.demo}`, {});
  $('from').value = ''; $('to').value = ''; $('dataset').value = 'wallet_ledger';
  notify(`Loaded ${result.record_count} synthetic transactions. Materialization is queued; results appear shortly.`);
  await loadTargets(result.target_id); await loadJobs();
}));
$('ingest').addEventListener('click', run($('ingest'), async () => {
  const result = await post(`/v1/targets/${state.target.id}/ingest`, { mode: 'incremental' });
  notify(`Fetch queued (${result.id}). Follow ingestion and materialization in Activity.`); await loadJobs();
}));
$('backfill').addEventListener('click', run($('backfill'), async () => {
  await post(`/v1/targets/${state.target.id}/ingest`, { mode: 'backfill' });
  notify('Older Solana history queued. Repeat to continue backwards through history.'); await loadJobs();
}));
$('query-form').addEventListener('submit', run($('query-form'), async () => { state.offset = 0; await loadRecords(); }));
$('dataset').addEventListener('change', run($('dataset'), async () => { state.offset = 0; await loadRecords(); }));
$('clear-filters').addEventListener('click', run($('clear-filters'), async () => { $('from').value = ''; $('to').value = ''; state.offset = 0; await loadRecords(); }));
for (const [id, delta] of [['previous', -PAGE_SIZE], ['next', PAGE_SIZE]]) $(id).addEventListener('click', run($(id), async () => { state.offset = Math.max(0, state.offset + delta); await loadRecords(); }));
for (const button of document.querySelectorAll('[data-export]')) button.addEventListener('click', run(button, async () => {
  const body = Object.fromEntries(filters()); for (const key of ['time_start', 'time_end']) if (key in body) body[key] = Number(body[key]);
  await post('/v1/export/dataset', { ...body, dataset: $('dataset').value, format: button.dataset.export });
  notify('Export queued. Download the artifact and its provenance here when completed.'); state.jobOffset = 0; panel('activity'); await loadJobs();
}));
$('import-form').addEventListener('submit', run($('import-form'), async () => {
  const file = $('import-file').files[0]; if (!file) throw new Error('Choose a JSON or JSONL file.');
  if (file.size > 1048576) throw new Error('File exceeds 1 MiB. Split it into smaller batches.');
  const text = await file.text(); let parsed;
  try { parsed = JSON.parse(text); } catch { try { parsed = text.split(/\r?\n/).filter(line => line.trim()).map(line => JSON.parse(line)); } catch { throw new Error('Invalid JSON/JSONL file.'); } }
  const records = Array.isArray(parsed) ? parsed : parsed.records;
  if (!Array.isArray(records)) throw new Error('Expected an array of records or {"records": […]}.');
  const result = await post(`/v1/targets/${state.target.id}/import`, { records });
  notify(`Imported ${result.record_count} records. ${result.materialization_job_id ? 'Materialization queued.' : 'Raw data is ready.'}`);
  $('import-file').value = ''; await loadJobs(); await loadRecords();
}));
$('refresh-jobs').addEventListener('click', run($('refresh-jobs'), loadJobs));
for (const [id, delta] of [['jobs-previous', -PAGE_SIZE], ['jobs-next', PAGE_SIZE]]) $(id).addEventListener('click', run($(id), async () => { state.jobOffset = Math.max(0, state.jobOffset + delta); await loadJobs(); }));
$('key-form').addEventListener('submit', run($('key-form'), async () => {
  const result = await post('/v1/api-keys', { name: $('key-name').value.trim() });
  $('new-key').value = result.key; $('new-key-box').hidden = false; $('key-name').value = ''; await loadKeys();
}));
$('copy-key').addEventListener('click', run($('copy-key'), async () => { await navigator.clipboard.writeText($('new-key').value); notify('Key copied. Store it securely.'); }));
setInterval(async () => {
  if (!state.session || state.pollBusy || document.hidden) return;
  state.pollBusy = true;
  try { await loadJobs(); }
  catch (error) { if (error.name !== 'AbortError') notify(`Refresh failed: ${error.message}`, true); }
  finally { state.pollBusy = false; }
}, 4000);
