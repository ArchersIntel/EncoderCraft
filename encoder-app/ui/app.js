'use strict';
// ASCII only on purpose: non-ASCII text uses \u escapes so encoding never matters.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const dialogApi = window.__TAURI__.dialog;

const $ = (id) => document.getElementById(id);
const LOCAL = 'local';
const DONE_STATES = ['completed', 'failed', 'cancelled'];
const VIDEO_EXTS = ['mov', 'mp4', 'm4v', 'mxf', 'mkv', 'avi', 'mpg', 'mpeg', 'mts', 'm2ts', 'webm', 'mj2'];

/** Build DOM with textContent only: file names and server errors are untrusted. */
function h(tag, attrs = {}, ...kids) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === 'class') el.className = v;
    else if (k === 'style' && typeof v === 'string') el.style.cssText = v; // CSSOM: allowed by the CSP, unlike a style attribute
    else if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
    else if (v !== false && v != null) el.setAttribute(k, v === true ? '' : v);
  }
  for (const kid of kids.flat()) if (kid != null && kid !== false) el.append(kid);
  return el;
}

const state = {
  nodes: [],
  active: LOCAL,
  jobs: new Map(),        // nodeId -> Map(jobId -> job)
  connection: new Map(),  // nodeId -> bool (live event stream)
  items: [],              // queue items not started yet
  selected: new Set(),    // row keys: "i:<uid>" or "j:<jobId>"
  anchor: null,
  presetSel: 'h264-web',
  log: [],
  lastState: new Map(),   // jobId -> last seen state (for the activity log)
  filmcraft: null,
  br: { path: '', entries: [], sel: null },
};

/** A job the queue counts as processing but that is still waiting for the shared renderer. */
const effState = (j) => {
  if (j.state !== 'processing') return j.state;
  const p = j.progress || {};
  if (p.waiting) return 'waiting';
  // A project sequence only really runs once FilmCraft starts reporting progress.
  if (j.spec.filmcraft && p.percent == null && !p.frame) return 'waiting';
  return 'processing';
};

const jobsOf = (id) => {
  if (!state.jobs.has(id)) state.jobs.set(id, new Map());
  return state.jobs.get(id);
};
const activeNode = () => state.nodes.find((n) => n.id === state.active);
const isLocal = () => (activeNode()?.kind ?? 'local') === 'local';
const errText = (e) => (typeof e === 'string' ? e : e?.message || JSON.stringify(e));

// ---------- path helpers (paths belong to the active node, which may not be this OS) ----------
const sepOf = (p) => (p.includes('\\') && !p.includes('/') ? '\\' : '/');
const baseName = (p) => p.split(/[\\/]/).filter(Boolean).pop() || p;
const extOf = (n) => (n.lastIndexOf('.') > 0 ? n.slice(n.lastIndexOf('.') + 1).toLowerCase() : '');
const stem = (name) => (name.lastIndexOf('.') > 0 ? name.slice(0, name.lastIndexOf('.')) : name);
const joinPath = (dir, name) => {
  const sep = sepOf(dir);
  return dir.endsWith(sep) ? dir + name : dir + sep + name;
};
const parentPath = (p) => {
  const sep = sepOf(p);
  const trimmed = p.length > 1 ? p.replace(/[\\/]+$/, '') : p;
  const i = trimmed.lastIndexOf(sep);
  if (i < 0) return '';
  if (i === 0) return sep;
  const up = trimmed.slice(0, i);
  return /^[A-Za-z]:$/.test(up) ? up + sep : up;
};
const safeName = (s) => s.replace(/[\\/:*?"<>|\u0000-\u001f]/g, '_').trim() || 'untitled';
const isVideoName = (n) => VIDEO_EXTS.includes(extOf(n));
const isProjectName = (n) => extOf(n) === 'fcproj';

function fmtDur(s) {
  s = Math.max(0, Math.round(s));
  const hh = Math.floor(s / 3600), mm = Math.floor((s % 3600) / 60), ss = s % 60;
  const p2 = (n) => String(n).padStart(2, '0');
  return hh ? `${hh}:${p2(mm)}:${p2(ss)}` : `${mm}:${p2(ss)}`;
}
const fmtSize = (n) => (n >= 1e9 ? (n / 1e9).toFixed(1) + ' GB' : n >= 1e6 ? (n / 1e6).toFixed(1) + ' MB' : n >= 1e3 ? Math.round(n / 1e3) + ' KB' : n + ' B');
const fmtFps = (f) => (Math.abs(f - Math.round(f)) < 0.001 ? String(Math.round(f)) : f.toFixed(3).replace(/0+$/, ''));
const clock = () => new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });

function toast(text, actionLabel, action) {
  const t = $('toast');
  t.replaceChildren(...[h('span', {}, text), actionLabel ? h('button', { onclick: () => { t.hidden = true; action(); } }, actionLabel) : null].filter(Boolean));
  t.hidden = false;
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => { t.hidden = true; }, 7000);
}
function flash(el, text, isErr) {
  el.textContent = text;
  el.classList.toggle('err', !!isErr);
}

// ---------- formats and presets ----------
const FORMATS = [
  { id: 'h264', name: 'H.264', ext: 'mp4', seq: true },
  { id: 'hevc', name: 'HEVC (H.265)', ext: 'mp4', seq: true },
  { id: 'av1', name: 'AV1', ext: 'mkv', seq: false },
  { id: 'copy', name: 'Copy streams', ext: 'mkv', seq: false },
];
const PRESETS = [
  { id: 'h264-high', format: 'h264', name: 'High Quality', quality: 17, desc: 'CRF 17' },
  { id: 'h264-web', format: 'h264', name: 'Web / Share', quality: 23, desc: 'CRF 23' },
  { id: 'h264-small', format: 'h264', name: 'Small File', quality: 28, desc: 'CRF 28' },
  { id: 'hevc-high', format: 'hevc', name: 'High Quality', quality: 20, desc: 'CRF 20' },
  { id: 'hevc-std', format: 'hevc', name: 'Standard', quality: 26, desc: 'CRF 26' },
  { id: 'hevc-small', format: 'hevc', name: 'Small File', quality: 31, desc: 'CRF 31' },
  { id: 'av1-std', format: 'av1', name: 'Standard', quality: 30, desc: 'CRF 30' },
  { id: 'av1-small', format: 'av1', name: 'Small File', quality: 35, desc: 'CRF 35' },
  { id: 'copy', format: 'copy', name: 'Re-wrap (no re-encode)', quality: 23, desc: 'MKV' },
];
const fmtOf = (id) => FORMATS.find((f) => f.id === id) || FORMATS[0];
const presetOf = (id) => PRESETS.find((p) => p.id === id);
const presetsFor = (format) => PRESETS.filter((p) => p.format === format);
const firstPreset = (format) => (presetsFor(format).find((p) => p.id.endsWith('web') || p.id.endsWith('std')) || presetsFor(format)[0]).id;

// ---------- queue items (not started yet) ----------
let uidCounter = 0;
const usedOutputs = () => new Set([
  ...state.items.map((i) => i.out.toLowerCase()),
  ...[...jobsOf(state.active).values()].filter((j) => !DONE_STATES.includes(j.state)).map((j) => j.spec.output.toLowerCase()),
]);

function outPathFor(item, dir) {
  const ext = fmtOf(item.format).ext;
  const taken = usedOutputs();
  let name = `${safeName(item.name)}.${ext}`, i = 2;
  while (taken.has(joinPath(dir, name).toLowerCase())) name = `${safeName(item.name)}-${i++}.${ext}`;
  return joinPath(dir, name);
}

function addItem(base) {
  const item = { uid: ++uidCounter, format: 'h264', preset: 'h264-web', outEdited: false, ...base };
  item.out = outPathFor(item, joinPath(item.srcDir, 'Encoded'));
  state.items.push(item);
  return item;
}

function setItemFormat(item, format, presetId) {
  item.format = format;
  item.preset = presetId || firstPreset(format);
  if (!item.outEdited) {
    const dir = parentPath(item.out);
    item.out = joinPath(dir, `${stem(baseName(item.out))}.${fmtOf(format).ext}`);
  } else {
    item.out = item.out.replace(/\.[^.\\/]+$/, '') + '.' + fmtOf(format).ext;
  }
}

function addFiles(paths) {
  let n = 0;
  for (const p of paths) {
    if (isProjectName(p)) { openProject(p); continue; }
    addItem({ kind: 'file', name: stem(baseName(p)), input: p, srcDir: parentPath(p) || '.' });
    n++;
  }
  if (n) { renderQueue(true); }
}

// ---------- queue table ----------
const rowEls = new Map();   // key -> <tr>
let queueSig = '';
let queueQueued = false;

function scheduleQueue() {
  if (queueQueued) return;
  queueQueued = true;
  requestAnimationFrame(() => { queueQueued = false; renderQueue(false); });
}

function allRows() {
  const jobs = [...jobsOf(state.active).values()].sort((a, b) => a.created_at.localeCompare(b.created_at));
  return [...jobs.map((j) => ({ key: `j:${j.id}`, job: j })), ...state.items.map((i) => ({ key: `i:${i.uid}`, item: i }))];
}

function renderQueue(force) {
  const rows = allRows();
  const sig = rows.map((r) => r.key).join('|');
  const structural = force || sig !== queueSig;
  queueSig = sig;
  const jobs = rows.filter((r) => r.job);
  const active = jobs.filter((r) => r.job.state === 'queued' || r.job.state === 'processing').length;
  $('empty').hidden = rows.length > 0;
  $('qTable').hidden = rows.length === 0;
  $('queueCount').textContent = rows.length ? `${rows.length} item${rows.length === 1 ? '' : 's'}${active ? ` \u{b7} ${active} active` : ''}` : '';

  if (structural) {
    for (const k of [...state.selected]) if (!rows.some((r) => r.key === k)) state.selected.delete(k);
    rowEls.clear();
    $('qBody').replaceChildren(...rows.map((r) => {
      const tr = r.job ? jobRow(r.job) : itemRow(r.item);
      tr.dataset.key = r.key;
      tr.classList.toggle('sel', state.selected.has(r.key));
      rowEls.set(r.key, tr);
      return tr;
    }));
  } else {
    for (const r of rows) if (r.job) patchJobRow(rowEls.get(r.key), r.job);
  }
  renderToolbar();
  renderEncoding();
}

function renderToolbar() {
  const n = activeNode();
  const ready = state.items.length;
  $('startQueue').disabled = !ready || !n?.connected;
  const hasActive = [...jobsOf(state.active).values()].some((j) => j.state === 'queued' || j.state === 'processing');
  $('stopQueue').disabled = !hasActive;
  $('removeSel').disabled = state.selected.size === 0;
  $('clearDone').disabled = ![...jobsOf(state.active).values()].some((j) => DONE_STATES.includes(j.state));
  $('applyPreset').disabled = !state.presetSel || ![...state.selected].some((k) => k.startsWith('i:'));
  $('addProject').disabled = !isLocal();
  $('addProject').title = isLocal() ? 'Add sequences from a FilmCraft project' : 'Project sequences render on this PC: switch the target to Local Machine';
}

function srcCell(name, sub, tag) {
  return h('td', {}, h('div', { class: 'src-name', title: name }, tag ? h('span', { class: 'tag' }, tag) : null, name), h('div', { class: 'src-sub', title: sub }, sub));
}

function itemRow(item) {
  const tr = h('tr', { class: 'row' });
  const seq = item.kind === 'sequence';
  const formats = FORMATS.filter((f) => !seq || f.seq);
  const fmtSel = h('select', { 'aria-label': `Format for ${item.name}`, onchange: (e) => { setItemFormat(item, e.target.value); renderQueue(true); } },
    formats.map((f) => h('option', { value: f.id, selected: f.id === item.format }, f.name)));
  const preSel = h('select', { 'aria-label': `Preset for ${item.name}`, onchange: (e) => { item.preset = e.target.value; } },
    presetsFor(item.format).map((p) => h('option', { value: p.id, selected: p.id === item.preset, title: p.desc }, p.name)));
  const outTd = h('td', {});
  const showOut = () => {
    outTd.replaceChildren(h('span', { class: 'outlink', title: `${item.out}\n(click to change)`, onclick: (e) => { e.stopPropagation(); editOut(); } }, item.out));
  };
  const editOut = () => {
    const input = h('input', { type: 'text', value: item.out, spellcheck: 'false', 'aria-label': 'Output file' });
    const done = (save) => {
      if (save && input.value.trim()) { item.out = input.value.trim(); item.outEdited = true; }
      showOut();
    };
    input.addEventListener('keydown', (e) => { if (e.key === 'Enter') done(true); if (e.key === 'Escape') done(false); });
    input.addEventListener('blur', () => done(true));
    outTd.replaceChildren(h('div', { class: 'outedit' }, input, h('button', { class: 'mini', onmousedown: (e) => e.preventDefault(), onclick: async () => {
      const dir = await pickFolder('Output folder', parentPath(item.out));
      if (dir) { item.out = joinPath(dir, baseName(item.out)); item.outEdited = true; }
      showOut();
    } }, '\u{2026}')));
    input.focus();
    input.select();
  };
  showOut();
  tr.append(
    srcCell(item.name, seq ? `${baseName(item.input)}  \u{b7}  ${item.specs}` : item.input, seq ? 'SEQUENCE' : null),
    h('td', {}, fmtSel), h('td', {}, preSel), outTd,
    h('td', {}, h('span', { class: 'pill ready' }, 'Ready')));
  return tr;
}

function jobMeta(j) {
  const fc = j.spec.filmcraft;
  const fmt = j.spec.video;
  const pre = PRESETS.find((p) => p.format === fmt && p.quality === j.spec.quality);
  return {
    name: fc ? (fc.sequence_name || `Sequence ${fc.sequence}`) : stem(baseName(j.spec.input)),
    sub: fc ? baseName(j.spec.input) : j.spec.input,
    tag: fc ? 'SEQUENCE' : null,
    fmtName: fmtOf(fmt).name,
    preName: fmt === 'copy' ? 'Re-wrap' : pre ? pre.name : `CRF ${j.spec.quality}`,
  };
}

function jobRow(j) {
  const m = jobMeta(j);
  const tr = h('tr', { class: 'row' });
  tr.append(srcCell(m.name, m.sub, m.tag), h('td', {}, m.fmtName), h('td', {}, m.preName),
    h('td', {}, h('span', { class: 'outplain', title: j.spec.output }, j.spec.output)), h('td', { class: 'st' }));
  patchJobRow(tr, j);
  return tr;
}

function etaOf(j) {
  const p = j.progress || {};
  if (p.eta_secs != null) return p.eta_secs;
  if (p.percent > 2 && j.started_at) {
    const elapsed = (Date.now() - Date.parse(j.started_at)) / 1000;
    return elapsed * (100 / p.percent - 1);
  }
  return null;
}

function patchJobRow(tr, j) {
  if (!tr) return;
  const td = tr.querySelector('td.st');
  const pct = j.progress?.percent;
  const processing = effState(j) === 'processing';
  const bar = (processing || j.state === 'completed')
    ? h('div', { class: 'bar' + (processing && pct == null ? ' indeterminate' : '') },
        h('div', { class: 'fill', style: processing && pct == null ? null : `width:${j.state === 'completed' ? 100 : pct ?? 0}%` }))
    : null;
  const stat = [];
  if (processing && pct != null) stat.push(`${pct.toFixed(0)}%`);
  const eta = processing ? etaOf(j) : null;
  if (eta != null) stat.push(`${fmtDur(eta)} left`);
  td.replaceChildren(...[
    h('span', { class: 'pill ' + effState(j) }, { queued: 'Waiting', waiting: 'Waiting', processing: 'Encoding' }[effState(j)] || j.state.charAt(0).toUpperCase() + j.state.slice(1)),
    bar, stat.length ? h('div', { class: 'pstat' }, stat.join(' \u{b7} ')) : null,
    j.error ? h('div', { class: 'errline', title: j.error }, j.error) : null].filter(Boolean));
}

// selection (click, ctrl-click toggle, shift-click range)
$('qBody').addEventListener('click', (e) => {
  if (e.target.closest('select, input, button, option, .outlink')) return;
  const tr = e.target.closest('tr.row');
  if (!tr) return;
  const key = tr.dataset.key;
  const order = allRows().map((r) => r.key);
  if (e.shiftKey && state.anchor && order.includes(state.anchor)) {
    const [a, b] = [order.indexOf(state.anchor), order.indexOf(key)].sort((x, y) => x - y);
    state.selected = new Set(order.slice(a, b + 1));
  } else if (e.ctrlKey || e.metaKey) {
    state.selected.has(key) ? state.selected.delete(key) : state.selected.add(key);
    state.anchor = key;
  } else {
    state.selected = new Set([key]);
    state.anchor = key;
  }
  rowEls.forEach((el, k) => el.classList.toggle('sel', state.selected.has(k)));
  renderToolbar();
});

// ---------- queue actions ----------
async function act(cmd, id) {
  try { await invoke(cmd, { nodeId: state.active, id }); } catch (e) { toast(errText(e)); }
}

$('removeSel').addEventListener('click', removeSelected);
async function removeSelected() {
  let blocked = 0;
  for (const key of [...state.selected]) {
    if (key.startsWith('i:')) {
      state.items = state.items.filter((i) => `i:${i.uid}` !== key);
    } else {
      const j = jobsOf(state.active).get(key.slice(2));
      if (!j) continue;
      if (DONE_STATES.includes(j.state)) await act('remove_job', j.id);
      else blocked++;
    }
  }
  state.selected.clear();
  renderQueue(true);
  if (blocked) toast(`${blocked} item${blocked === 1 ? ' is' : 's are'} still encoding. Press Stop first.`);
}

$('clearDone').addEventListener('click', async () => {
  for (const j of [...jobsOf(state.active).values()]) if (DONE_STATES.includes(j.state)) await act('remove_job', j.id);
});

$('stopQueue').addEventListener('click', async () => {
  for (const j of [...jobsOf(state.active).values()]) if (j.state === 'queued' || j.state === 'processing') await act('cancel_job', j.id);
});

const specOf = (item) => {
  const fmt = fmtOf(item.format).id;
  return {
    input: item.input,
    output: item.out,
    video: fmt,
    accel: 'auto',
    quality: (presetOf(item.preset) || {}).quality ?? 23,
    audio: fmt === 'copy' ? { codec: 'copy' } : { codec: 'aac', bitrate_kbps: 160 },
    overwrite: $('overwrite').checked,
    filmcraft: item.kind === 'sequence' ? { project: item.input, sequence: item.seqId, sequence_name: item.name } : null,
  };
};

$('startQueue').addEventListener('click', startQueue);
async function startQueue() {
  if ($('startQueue').disabled) return;
  const local = isLocal();
  const runnable = state.items.filter((i) => local || i.kind !== 'sequence');
  const skipped = state.items.length - runnable.length;
  if (skipped) toast(`${skipped} project sequence${skipped === 1 ? '' : 's'} left in the queue: sequences render on this PC. Switch the target to Local Machine to run them.`);
  if (!runnable.length) return;
  $('startQueue').disabled = true;
  try {
    const jobs = await invoke('submit_jobs', { nodeId: state.active, specs: runnable.map(specOf) });
    jobs.forEach((j) => jobsOf(state.active).set(j.id, j));
    const gone = new Set(runnable.map((i) => i.uid));
    state.items = state.items.filter((i) => !gone.has(i.uid));
  } catch (e) { toast(errText(e)); }
  renderQueue(true);
}

$('outAll').addEventListener('click', async () => {
  const targets = state.items.filter((i) => state.selected.size === 0 || state.selected.has(`i:${i.uid}`));
  if (!targets.length) return toast('Add items to the queue first.');
  const dir = await pickFolder('Output folder for the queue', parentPath(targets[0].out));
  if (!dir) return;
  for (const i of targets) { i.out = joinPath(dir, baseName(i.out)); i.outEdited = true; }
  renderQueue(true);
});

// persistent "overwrite" option, injected next to the queue tools
{
  let saved = false;
  try { saved = localStorage.getItem('overwrite') === '1'; } catch { /* ignore */ }
  const cb = h('input', { id: 'overwrite', type: 'checkbox', checked: saved, onchange: () => { try { localStorage.setItem('overwrite', cb.checked ? '1' : '0'); } catch { /* ignore */ } } });
  document.querySelector('.head-tools').prepend(h('label', { class: 'check', title: 'Replace output files that already exist' }, cb, ' Overwrite'));
}

// ---------- Encoding panel ----------
function logLine(text, cls) {
  state.log.push({ t: clock(), text, cls });
  if (state.log.length > 300) state.log.shift();
  renderLog();
}
function renderLog() {
  const ul = $('encLog');
  ul.replaceChildren(...state.log.slice(-60).map((l) => h('li', { class: l.cls || '' }, h('span', { class: 't' }, l.t), l.text)));
  ul.scrollTop = ul.scrollHeight;
}
function renderEncoding() {
  const jobs = [...jobsOf(state.active).values()];
  const running = jobs.filter((j) => effState(j) === 'processing');
  const waiting = jobs.filter((j) => j.state === 'queued' || effState(j) === 'waiting').length;
  const host = $('encActive');
  if (!running.length) {
    host.replaceChildren(h('div', { class: 'enc-idle' }, waiting ? `Waiting to start ${waiting} item${waiting === 1 ? '' : 's'}\u{2026}` : 'Idle'));
    return;
  }
  host.replaceChildren(...running.map((j) => {
    const m = jobMeta(j);
    const pct = j.progress?.percent;
    const eta = etaOf(j);
    const bits = [];
    if (pct != null) bits.push(`${pct.toFixed(1)}%`);
    if (j.progress?.frame) bits.push(`frame ${j.progress.frame}`);
    if (j.progress?.fps) bits.push(`${j.progress.fps.toFixed(1)} fps`);
    if (j.progress?.speed) bits.push(`${j.progress.speed.toFixed(2)}\u{d7}`);
    if (eta != null) bits.push(`${fmtDur(eta)} remaining`);
    return h('div', { class: 'enc-item' },
      h('div', { class: 'enc-title' }, m.name),
      h('div', { class: 'bar thick' + (pct == null ? ' indeterminate' : '') }, h('div', { class: 'fill', style: pct == null ? null : `width:${pct}%` })),
      h('div', { class: 'pstat' }, bits.join(' \u{b7} ') || 'Starting\u{2026}'));
  }));
}

/** Writes one activity-log line when a job changes state. */
function trackJob(j, quiet) {
  const prev = state.lastState.get(j.id);
  const eff = effState(j);
  state.lastState.set(j.id, eff);
  if (quiet || prev === eff) return;
  const m = jobMeta(j);
  if (eff === 'processing') logLine(`Started ${m.name}`);
  else if (eff === 'waiting') { /* not started yet */ }
  else if (j.state === 'completed') {
    const secs = j.started_at && j.finished_at ? (Date.parse(j.finished_at) - Date.parse(j.started_at)) / 1000 : null;
    logLine(`Finished ${m.name}${secs != null ? ` in ${fmtDur(secs)}` : ''}  ->  ${j.spec.output}`, 'ok');
  } else if (j.state === 'failed') logLine(`FAILED ${m.name}: ${j.error || 'unknown error'}`, 'bad');
  else if (j.state === 'cancelled') logLine(`Cancelled ${m.name}`);
}

// ---------- node info / banner ----------
function renderTargets() {
  $('targetSelect').replaceChildren(...state.nodes.map((n) =>
    h('option', { value: n.id, selected: n.id === state.active },
      n.kind === 'local' ? 'Local Machine' : `${n.name}${n.connected ? '' : ' (disconnected)'}`)));
  renderNodeInfo();
}

function showBanner(text, kind, actionLabel, action) {
  const b = $('banner');
  b.className = 'banner' + (kind === 'err' ? ' err' : '');
  b.replaceChildren(...[h('span', {}, text), actionLabel ? h('button', { onclick: action }, actionLabel) : null].filter(Boolean));
  b.hidden = false;
}

function renderNodeInfo() {
  const n = activeNode();
  const chips = $('nodeChips');
  chips.replaceChildren();
  $('banner').hidden = true;
  if (n?.info) {
    const hw = n.info.hardware;
    chips.append(
      h('span', { class: 'chip ' + (hw.nvenc ? 'ok' : '') }, hw.nvenc ? 'NVENC' : 'CPU only'),
      h('span', { class: 'chip' }, `${n.info.max_concurrent} at a time`));
    if (!hw.ffmpeg_version) {
      showBanner(`FFmpeg was not found on ${n.kind === 'local' ? 'this machine' : 'the server'}; media files cannot be encoded until it is installed.`, 'warn');
    }
  }
  if (n && n.kind === 'remote' && !n.connected) {
    showBanner(`Not connected to ${n.name}.`, 'warn', 'Connect', () => connectFlow(n));
  } else if (n && n.connected && state.connection.get(n.id) === false) {
    showBanner('Lost the live connection to this server. Retrying\u{2026}', 'warn', 'Reconnect', () => reconnect(n));
  }
  renderToolbar();
}

// ---------- node management ----------
async function refreshNodes() {
  state.nodes = await invoke('list_nodes');
  if (!state.nodes.some((n) => n.id === state.active)) state.active = LOCAL;
  renderTargets();
  renderServers();
}

async function loadJobs(nodeId) {
  const n = state.nodes.find((x) => x.id === nodeId);
  if (!n?.connected) { jobsOf(nodeId).clear(); renderQueue(true); return; }
  try {
    const jobs = await invoke('list_jobs', { nodeId });
    const m = jobsOf(nodeId);
    m.clear();
    jobs.forEach((j) => { m.set(j.id, j); trackJob(j, true); });
  } catch (e) { showBanner(errText(e), 'err'); }
  renderQueue(true);
}

async function selectNode(id) {
  state.active = id;
  try { localStorage.setItem('activeNode', id); } catch { /* storage may be unavailable */ }
  state.selected.clear();
  renderTargets();
  await loadJobs(id);
  brLoad('');
}
$('targetSelect').addEventListener('change', (e) => selectNode(e.target.value));

async function connectFlow(node) {
  if (!node.uses_key) return promptSecret(node);
  try { await connectWith(node, null); } catch (e) { promptSecret(node, errText(e)); }
}
async function connectWith(node, secret) {
  await invoke('connect_node', { nodeId: node.id, secret });
  await refreshNodes();
  if (state.active === node.id) { await loadJobs(node.id); brLoad(''); }
}
async function reconnect(node) {
  try { await invoke('disconnect_node', { nodeId: node.id }); } catch { /* ignore */ }
  await refreshNodes();
  connectFlow(state.nodes.find((n) => n.id === node.id) ?? node);
}

function promptSecret(node, error) {
  $('secretTitle').textContent = `Connect to ${node.name}`;
  $('secretLabel').textContent = node.uses_key ? 'Key passphrase' : `Password for ${node.username}@${node.host}`;
  $('secretInput').value = '';
  flash($('secretMsg'), error || '', true);
  $('secretGo').onclick = async (ev) => {
    ev.preventDefault();
    $('secretGo').disabled = true;
    flash($('secretMsg'), 'Connecting\u{2026}', false);
    try {
      await connectWith(node, $('secretInput').value);
      $('secretInput').value = '';
      $('secretDlg').close();
    } catch (e) { flash($('secretMsg'), errText(e), true); }
    $('secretGo').disabled = false;
  };
  $('secretDlg').showModal();
  $('secretInput').focus();
}

function renderServers() {
  const remotes = state.nodes.filter((n) => n.kind === 'remote');
  $('serverList').replaceChildren(...(remotes.length ? remotes.map((n) => h('li', {},
    h('div', { class: 'meta' },
      h('div', {}, n.name),
      h('div', {}, `${n.username}@${n.host} \u{b7} ${n.connected ? 'connected' : 'disconnected'}`)),
    n.connected
      ? h('button', { class: 'mini', onclick: async () => { await invoke('disconnect_node', { nodeId: n.id }); await refreshNodes(); await loadJobs(state.active); } }, 'Disconnect')
      : h('button', { class: 'mini', onclick: () => { $('serversDlg').close(); connectFlow(n); } }, 'Connect'),
    h('button', { class: 'mini', onclick: async () => {
      if (!confirm(`Forget "${n.name}"? This removes it from EncoderCraft but does not uninstall anything from the server.`)) return;
      await invoke('remove_node', { nodeId: n.id });
      await refreshNodes();
      await loadJobs(state.active);
    } }, 'Forget'))) : [h('li', {}, h('div', { class: 'meta' }, h('div', {}, 'No servers yet'), h('div', {}, 'Deploy one to offload encoding to another machine.')))]));
}

// ---------- pickers (native on this PC, in-app browser for servers) ----------
let browseResolve = null;
const browse = { path: '', mode: 'folder', checked: new Set() };

function openBrowser({ mode, title, start }) {
  browse.mode = mode;
  browse.checked = new Set();
  $('browseTitle').textContent = title;
  $('browseSelect').textContent = mode === 'folder' ? 'Select this folder' : 'Select files';
  flash($('browseMsg'), '', true);
  return new Promise((resolve) => {
    browseResolve = resolve;
    $('browseDlg').showModal();
    loadDir(start || '');
  });
}
async function loadDir(path) {
  flash($('browseMsg'), 'Loading\u{2026}', false);
  try {
    const l = await invoke('list_dir', { nodeId: state.active, path: path || null });
    browse.path = l.path;
    $('browsePath').value = l.path;
    flash($('browseMsg'), l.truncated ? 'Showing the first 5000 entries.' : '', false);
    const rows = l.entries.map((e) => {
      const full = browse.path === '' ? e.name : joinPath(browse.path, e.name);
      if (e.is_dir) return h('li', { class: 'dir', onclick: () => loadDir(full) }, '\u{1f4c1} ', h('span', {}, e.name));
      return h('li', {},
        browse.mode === 'files' ? h('input', { type: 'checkbox', onchange: (ev) => { ev.target.checked ? browse.checked.add(full) : browse.checked.delete(full); } }) : null,
        h('span', {}, e.name), h('span', { class: 'sz' }, fmtSize(e.size)));
    });
    $('browseList').replaceChildren(...(rows.length ? rows : [h('li', { class: 'none' }, 'Empty folder')]));
    $('browseSelect').disabled = browse.mode === 'folder' && browse.path === '';
  } catch (e) { flash($('browseMsg'), errText(e), true); }
}
function closeBrowser(value) {
  $('browseDlg').close();
  if (browseResolve) browseResolve(value);
  browseResolve = null;
}
$('browseDlg').addEventListener('close', () => { if (browseResolve) { browseResolve(null); browseResolve = null; } });
$('browseCancel').addEventListener('click', (e) => { e.preventDefault(); closeBrowser(null); });
$('browseUp').addEventListener('click', (e) => { e.preventDefault(); loadDir(browse.path === '' ? '' : parentPath(browse.path)); });
$('browseGo').addEventListener('click', (e) => { e.preventDefault(); loadDir($('browsePath').value.trim()); });
$('browsePath').addEventListener('keydown', (e) => { if (e.key === 'Enter') { e.preventDefault(); loadDir($('browsePath').value.trim()); } });
$('browseSelect').addEventListener('click', (e) => { e.preventDefault(); closeBrowser(browse.mode === 'folder' ? browse.path : [...browse.checked]); });

async function pickFolder(title, start) {
  if (isLocal()) return (await dialogApi.open({ directory: true, title, defaultPath: start || undefined })) || null;
  return openBrowser({ mode: 'folder', title, start });
}
async function pickFiles(title, { multiple = true, start, filters } = {}) {
  if (isLocal()) {
    const p = await dialogApi.open({ multiple, title, defaultPath: start || undefined, filters });
    return p ? [].concat(p) : [];
  }
  const sel = await openBrowser({ mode: 'files', title, start });
  return sel ? (multiple ? sel : sel.slice(0, 1)) : [];
}

// ---------- Media Browser ----------
async function brLoad(path) {
  flash($('brMsg'), '', false);
  try {
    const l = await invoke('list_dir', { nodeId: state.active, path: path || null });
    state.br = { path: l.path, entries: l.entries, sel: null };
    $('brPath').value = l.path;
    try { localStorage.setItem(`br:${state.active}`, l.path); } catch { /* ignore */ }
    renderBrowser();
  } catch (e) {
    state.br = { path: '', entries: [], sel: null };
    renderBrowser();
    $('brMsg').textContent = errText(e);
  }
}
function renderBrowser() {
  const rows = state.br.entries
    .filter((e) => e.is_dir || isVideoName(e.name) || isProjectName(e.name))
    .map((e) => {
      const full = state.br.path === '' ? e.name : joinPath(state.br.path, e.name);
      const proj = !e.is_dir && isProjectName(e.name);
      const li = h('li', { class: state.br.sel === full ? 'sel' : '', title: full },
        h('span', { class: 'ic' }, e.is_dir ? '\u{1f4c1}' : proj ? '\u{1f3ac}' : '\u{1f39e}'),
        h('span', { class: 'nm' }, e.is_dir && state.br.path === '' ? e.name : baseName(e.name)),
        e.is_dir ? null : h('span', { class: 'sz' }, fmtSize(e.size)));
      li.addEventListener('click', () => { state.br.sel = full; $('brList').querySelectorAll('li').forEach((x) => x.classList.remove('sel')); li.classList.add('sel'); });
      li.addEventListener('dblclick', () => brOpen(full, e.is_dir));
      li.dataset.full = full;
      li.dataset.dir = e.is_dir ? '1' : '';
      return li;
    });
  $('brList').replaceChildren(...(rows.length ? rows : [h('li', { class: 'dim' }, 'No folders or video files here')]));
}
function brOpen(full, isDir) {
  if (isDir) return brLoad(full);
  addFiles([full]);
}
$('brUp').addEventListener('click', () => brLoad(state.br.path ? parentPath(state.br.path) : ''));
$('brPath').addEventListener('keydown', (e) => { if (e.key === 'Enter') brLoad($('brPath').value.trim()); });
$('brList').addEventListener('keydown', (e) => {
  if (e.key !== 'Enter' || !state.br.sel) return;
  const li = [...$('brList').children].find((x) => x.dataset.full === state.br.sel);
  brOpen(state.br.sel, !!li?.dataset.dir);
});

// ---------- Preset Browser ----------
function renderPresets() {
  const rows = [];
  for (const f of FORMATS) {
    rows.push(h('li', { class: 'group' }, f.name));
    for (const p of presetsFor(f.id)) {
      const li = h('li', { class: state.presetSel === p.id ? 'sel' : '', title: 'Double-click to apply to the selected queue items' },
        h('span', { class: 'nm' }, p.name), h('span', { class: 'pdesc' }, p.desc));
      li.addEventListener('click', () => { state.presetSel = p.id; renderPresets(); renderToolbar(); });
      li.addEventListener('dblclick', () => { state.presetSel = p.id; applyPreset(); });
      rows.push(li);
    }
  }
  $('presetList').replaceChildren(...rows);
}
$('applyPreset').addEventListener('click', applyPreset);
function applyPreset() {
  const p = presetOf(state.presetSel);
  if (!p) return;
  const targets = state.items.filter((i) => state.selected.has(`i:${i.uid}`));
  if (!targets.length) return toast('Select waiting items in the queue first, then apply a preset.');
  let skipped = 0;
  for (const i of targets) {
    if (i.kind === 'sequence' && !fmtOf(p.format).seq) { skipped++; continue; }
    setItemFormat(i, p.format, p.id);
  }
  renderQueue(true);
  if (skipped) toast(`${fmtOf(p.format).name} is not available for project sequences (${skipped} skipped).`);
}

// ---------- adding sources ----------
$('addFiles').addEventListener('click', async () => {
  const picked = await pickFiles('Add media to the queue', {
    start: state.br.path || undefined,
    filters: [{ name: 'Video', extensions: VIDEO_EXTS }, { name: 'FilmCraft project', extensions: ['fcproj'] }, { name: 'All files', extensions: ['*'] }],
  });
  if (picked.length) addFiles(picked);
});
$('addProject').addEventListener('click', async () => {
  const [path] = await pickFiles('Open FilmCraft project', { multiple: false, filters: [{ name: 'FilmCraft project', extensions: ['fcproj'] }, { name: 'All files', extensions: ['*'] }] });
  if (path) openProject(path);
});

const proj = { summary: null, selected: new Set(), showInternal: false, path: '' };

async function openProject(path) {
  if (!isLocal()) return toast('Project sequences render on this PC with FilmCraft. Switch the target to Local Machine first.');
  if (!state.filmcraft?.found) {
    state.filmcraft = await invoke('filmcraft_info');
    if (!state.filmcraft.found) return toast('FilmCraft was not found on this PC (looked for filmcraft-cli). Install FilmCraft or set FILMCRAFT_CLI to its path.');
  }
  try {
    proj.summary = await invoke('import_project', { path });
  } catch (e) { return toast(errText(e)); }
  proj.path = path;
  proj.showInternal = false;
  $('projInternal').checked = false;
  const s = proj.summary;
  proj.selected = new Set(s.sequences.filter((q) => !q.nested_in_other && !q.multicam_source).map((q) => q.id));
  $('projTitle').textContent = `Add sequences from ${s.name || baseName(path)}`;
  $('projMeta').textContent = `${s.sequences.length} sequence${s.sequences.length === 1 ? '' : 's'} \u{b7} saved by ${s.generator || 'FilmCraft'} \u{b7} rendered by FilmCraft ${state.filmcraft.version ? state.filmcraft.version.replace(/^filmcraft-cli\s*/, '') : ''}`;
  $('projWarnings').hidden = !s.warnings.length;
  $('projWarnings').replaceChildren(...s.warnings.map((w) => h('li', {}, w)));
  flash($('projMsg'), '', true);
  renderProjSeqs();
  $('projDlg').showModal();
}

function projVisible() {
  return proj.summary.sequences.filter((q) => proj.showInternal || !(q.nested_in_other || q.multicam_source));
}
function renderProjSeqs() {
  const vis = projVisible();
  $('projSeqs').replaceChildren(...(vis.length ? vis.map((q) => {
    const media = q.media.total ? `${q.media.total} source file${q.media.total === 1 ? '' : 's'}` : 'no media';
    return h('li', {},
      h('input', { type: 'checkbox', checked: proj.selected.has(q.id), 'aria-label': q.name, onchange: (e) => { e.target.checked ? proj.selected.add(q.id) : proj.selected.delete(q.id); updateProjAdd(); } }),
      h('div', { class: 'meta' },
        h('div', { class: 'nm' }, q.name),
        h('div', { class: 'sub' }, `${q.width}\u{d7}${q.height} \u{b7} ${fmtFps(q.fps)} fps \u{b7} ${fmtDur(q.duration_secs)} \u{b7} `,
          q.media.missing ? h('span', { class: 'bad' }, `${q.media.missing} of ${media} offline`) : media)));
  }) : [h('li', {}, h('div', { class: 'meta sub' }, 'This project has no sequences.'))]));
  $('projAll').checked = vis.length > 0 && vis.every((q) => proj.selected.has(q.id));
  updateProjAdd();
}
function updateProjAdd() {
  const n = projVisible().filter((q) => proj.selected.has(q.id)).length;
  $('projAdd').textContent = n ? `Add ${n} sequence${n === 1 ? '' : 's'} to queue` : 'Add to queue';
  $('projAdd').disabled = n === 0;
}
$('projAll').addEventListener('change', (e) => {
  projVisible().forEach((q) => (e.target.checked ? proj.selected.add(q.id) : proj.selected.delete(q.id)));
  renderProjSeqs();
});
$('projInternal').addEventListener('change', (e) => { proj.showInternal = e.target.checked; renderProjSeqs(); });
$('projAdd').addEventListener('click', (e) => {
  e.preventDefault();
  const dir = parentPath(proj.path);
  let n = 0;
  for (const q of projVisible()) {
    if (!proj.selected.has(q.id)) continue;
    addItem({
      kind: 'sequence', name: q.name, input: proj.path, seqId: q.id, srcDir: dir,
      specs: `${q.width}\u{d7}${q.height} \u{b7} ${fmtFps(q.fps)} fps \u{b7} ${fmtDur(q.duration_secs)}`,
    });
    n++;
  }
  $('projDlg').close();
  renderQueue(true);
  toast(`Added ${n} sequence${n === 1 ? '' : 's'}. Press Start Queue to render them.`);
});

// drag and drop files / projects onto the window (this PC only)
(async () => {
  try {
    await listen('tauri://drag-enter', () => document.body.classList.add('dropping'));
    await listen('tauri://drag-leave', () => document.body.classList.remove('dropping'));
    await listen('tauri://drag-drop', ({ payload }) => {
      document.body.classList.remove('dropping');
      const paths = (payload?.paths || []).filter((p) => isVideoName(p) || isProjectName(p));
      if (!isLocal()) return toast('Dropping files works for the Local Machine. For a server, pick files in the Media Browser.');
      if (paths.length) addFiles(paths);
    });
  } catch { /* drag and drop is optional */ }
})();

// keyboard
document.addEventListener('keydown', (e) => {
  const typing = /^(INPUT|TEXTAREA|SELECT)$/.test(document.activeElement?.tagName || '');
  if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) { e.preventDefault(); startQueue(); }
  else if (e.key === 'Delete' && !typing && state.selected.size) removeSelected();
});

// ---------- live events ----------
listen('node-event', ({ payload: ev }) => {
  const m = jobsOf(ev.node_id);
  const mine = ev.node_id === state.active;
  switch (ev.type) {
    case 'snapshot': m.clear(); ev.jobs.forEach((j) => { m.set(j.id, j); trackJob(j, true); }); break;
    case 'job_updated': m.set(ev.job.id, ev.job); if (mine) trackJob(ev.job, false); break;
    case 'job_removed': m.delete(ev.id); state.lastState.delete(ev.id); break;
    case 'lagged': loadJobs(ev.node_id); break;
    case 'connection':
      state.connection.set(ev.node_id, ev.connected);
      if (mine) renderNodeInfo();
      break;
  }
  if (mine) scheduleQueue();
});

// ---------- servers dialog + deploy wizard ----------
$('serversBtn').addEventListener('click', async () => { await refreshNodes(); $('serversDlg').showModal(); });

const authMode = () => document.querySelector('input[name=dAuth]:checked').value;
document.querySelectorAll('input[name=dAuth]').forEach((r) => r.addEventListener('change', () => {
  $('dPwRow').hidden = authMode() !== 'password';
  $('dKeyRow').hidden = authMode() !== 'key';
  resetTrust();
}));
['dHost', 'dPort'].forEach((id) => $(id).addEventListener('input', resetTrust));

let fingerprint = null;
function resetTrust() {
  $('trustBox').hidden = true;
  $('trustOk').checked = false;
  $('startDeployBtn').hidden = true;
  $('startDeployBtn').disabled = true;
  $('checkHostBtn').hidden = false;
  fingerprint = null;
}

$('deployBtn').addEventListener('click', async () => {
  $('serversDlg').close();
  $('deployForm').hidden = false;
  $('deployProgress').hidden = true;
  flash($('deployMsg'), '', true);
  resetTrust();
  if (!$('dBinary').value) {
    try { $('dBinary').value = (await invoke('default_daemon_binary')) || ''; } catch { /* optional */ }
  }
  $('deployDlg').showModal();
});
$('dBrowseKey').addEventListener('click', async (ev) => {
  ev.preventDefault();
  const p = await dialogApi.open({ title: 'Choose SSH private key' });
  if (p) $('dKey').value = p;
});
$('dBrowseBin').addEventListener('click', async (ev) => {
  ev.preventDefault();
  const p = await dialogApi.open({ title: 'Choose encraft-d (Linux x86_64) binary' });
  if (p) $('dBinary').value = p;
});

function validateDeployForm() {
  if (!$('dHost').value.trim()) return 'Enter the server IP or hostname.';
  if (!$('dUser').value.trim()) return 'Enter the SSH username.';
  if (authMode() === 'password' && !$('dPassword').value) return 'Enter the SSH password.';
  if (authMode() === 'key' && !$('dKey').value.trim()) return 'Choose a private key file.';
  if (!$('dBinary').value.trim()) return 'Choose the encraft-d binary to upload.';
  return null;
}

$('checkHostBtn').addEventListener('click', async () => {
  const bad = validateDeployForm();
  if (bad) return flash($('deployMsg'), bad, true);
  $('checkHostBtn').disabled = true;
  flash($('deployMsg'), 'Contacting server\u{2026}', false);
  try {
    fingerprint = await invoke('probe_host', { host: $('dHost').value.trim(), port: Number($('dPort').value) });
    $('fpValue').textContent = fingerprint;
    $('trustBox').hidden = false;
    $('checkHostBtn').hidden = true;
    $('startDeployBtn').hidden = false;
    flash($('deployMsg'), '', false);
  } catch (e) { flash($('deployMsg'), errText(e), true); }
  $('checkHostBtn').disabled = false;
});
$('trustOk').addEventListener('change', () => { $('startDeployBtn').disabled = !$('trustOk').checked; });

const STEP_NAMES = ['Checking remote host', 'Checking FFmpeg', 'Creating service user and directories',
  'Uploading encraft-d', 'Writing configuration and systemd unit', 'Starting service'];

$('startDeployBtn').addEventListener('click', async () => {
  const key = authMode() === 'key';
  const req = {
    name: $('dName').value,
    host: $('dHost').value.trim(),
    port: Number($('dPort').value),
    username: $('dUser').value.trim(),
    password: key ? null : $('dPassword').value,
    keyPath: key ? $('dKey').value.trim() : null,
    passphrase: key ? $('dPass').value : null,
    hostKey: fingerprint,
    binaryPath: $('dBinary').value.trim(),
    mediaRoots: $('dRoots').value.split('\n').map((s) => s.trim()).filter(Boolean),
    installFfmpeg: $('dFfmpeg').checked,
    sudoPassword: $('dSudo').value || null,
  };
  $('deployForm').hidden = true;
  $('deployProgress').hidden = false;
  $('deployDone').hidden = true;
  $('deployLog').textContent = '';
  $('uploadBar').hidden = true;
  flash($('deployResult'), '', false);
  $('steps').replaceChildren(...STEP_NAMES.map((n) => h('li', {}, n)));
  try {
    const node = await invoke('deploy_server', { req });
    $('dPassword').value = ''; $('dPass').value = ''; $('dSudo').value = '';
    flash($('deployResult'), `${node.name} is deployed and connected.`, false);
    [...$('steps').children].forEach((li) => { li.className = 'done'; });
    $('deployDone').hidden = false;
    await refreshNodes();
    await selectNode(node.id);
  } catch (e) {
    $('deployProgress').hidden = true;
    $('deployForm').hidden = false;
    flash($('deployMsg'), errText(e), true);
    if (/sudo needs a password|sudo rejected/.test(errText(e))) { $('dSudo').value = ''; $('dSudo').focus(); }
  }
});
$('deployDone').addEventListener('click', () => $('deployDlg').close());

listen('deploy-progress', ({ payload: ev }) => {
  const steps = [...$('steps').children];
  if (ev.type === 'step') {
    steps.forEach((li, i) => { li.className = i < ev.index - 1 ? 'done' : i === ev.index - 1 ? 'active' : ''; });
  } else if (ev.type === 'log') {
    $('deployLog').textContent += ev.message + '\n';
  } else if (ev.type === 'upload') {
    $('uploadBar').hidden = false;
    $('uploadBar').firstElementChild.style.width = `${ev.total ? (ev.sent / ev.total) * 100 : 0}%`;
  }
});

// ---------- boot ----------
(async function init() {
  try { state.active = localStorage.getItem('activeNode') || LOCAL; } catch { /* ignore */ }
  renderPresets();
  renderLog();
  try {
    await refreshNodes();
    await loadJobs(state.active);
    state.filmcraft = await invoke('filmcraft_info');
  } catch (e) { showBanner(errText(e), 'err'); }
  let start = '';
  try { start = localStorage.getItem(`br:${state.active}`) || ''; } catch { /* ignore */ }
  await brLoad(start);
  renderQueue(true);
})();
