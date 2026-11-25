'use strict';

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const nowS = () => Math.floor(Date.now() / 1000);
const timeFmt = new Intl.DateTimeFormat('en-GB', { hour: '2-digit', minute: '2-digit', hourCycle: 'h23' });
const dayFmt = new Intl.DateTimeFormat('en-GB', { weekday: 'short' });
const dateFmt = new Intl.DateTimeFormat('en-GB', { weekday: 'short', day: 'numeric', month: 'short' });
const hm = (ts) => timeFmt.format(new Date(ts * 1000));
const dow = (ts) => dayFmt.format(new Date(ts * 1000));
const sameDay = (ts) => new Date(ts * 1000).toDateString() === new Date().toDateString();

let snap = null;
let lastPingSeen = undefined;
let partyUntil = 0;
let quipUntil = 0;
let quip = '';

// 0 plenty (≥50% left) · 1 half · 2 low (<25%) · 3 nearly out (<10%)
function level(used) {
  const left = 100 - Math.min(100, Math.max(0, used));
  return left >= 50 ? 0 : left >= 25 ? 1 : left >= 10 ? 2 : 3;
}

function untilText(sec) {
  if (sec <= 0) return 'now';
  const m = Math.ceil(sec / 60);
  const d = Math.floor(m / 1440);
  const h = Math.floor((m % 1440) / 60);
  if (d) return `in ${d}d ${h}h`;
  return h ? `in ${h}h ${m % 60}m` : `in ${m}m`;
}

function agoText(ts) {
  const d = nowS() - ts;
  if (d < 45) return 'just now';
  if (d < 3600) return `${Math.round(d / 60)} min ago`;
  return `at ${hm(ts)}`;
}

function moodOf(s, t) {
  if (s.pinging || Date.now() < partyUntil) return 'party';
  if (s.status === 'signed_out' || (!s.session && s.status !== 'ok')) return 'puzzled';
  const ss = s.session;
  if (!ss || !ss.resets_at || ss.resets_at <= t || ss.pct >= 100) return 'sleepy';
  return ['happy', 'calm', 'worried', 'panic'][level(ss.pct)];
}

function sayText(s, t) {
  if (Date.now() < quipUntil) return quip;
  if (Date.now() < partyUntil) return 'New session started. Fresh 5 hours.';
  if (s.status === 'signed_out') return "Can't find your Claude login. Sign in to Claude Code.";
  if (s.pinging) return 'Saying hi to Claude…';
  const ss = s.session;
  if (!ss) return s.status === 'stale' ? "Can't reach Anthropic right now." : 'Loading usage…';
  const active = ss.resets_at && ss.resets_at > t;
  if (!active) return s.settings.auto_session ? 'Session ended. Starting a new one…' : 'No session. Your next message starts one.';
  if (ss.pct >= 100) return `Limit reached. Resets at ${hm(ss.resets_at)}.`;
  const wk = s.weekly;
  if (wk && 100 - wk.pct < 10) return `Weekly limit almost used up. Resets ${dow(wk.resets_at)}.`;
  const left = 100 - ss.pct;
  if (left >= 75) return 'Plenty left in this session.';
  if (left >= 50) return 'Going well. Over half left.';
  if (left >= 25) return 'Past halfway. Pace yourself.';
  if (left >= 10) return `Running low: ${Math.round(left)}% left.`;
  return `Almost out. Resets at ${hm(ss.resets_at)}.`;
}

const QUIPS = ['Hello!', 'Keep shipping.', "I'm watching the clock.", 'Still here.', "I'll say hi at the reset."];

// ---------- numbers count up/down smoothly
const counters = {};
function setNum(id, value) {
  const el = $(id);
  if (value == null) {
    el.textContent = '—';
    counters[id] = null;
    return;
  }
  const from = counters[id] ?? 0;
  counters[id] = value;
  if (from === value) {
    el.textContent = value;
    return;
  }
  const t0 = performance.now();
  const step = (now) => {
    const k = Math.min(1, (now - t0) / 700);
    el.textContent = Math.round(from + (value - from) * (1 - Math.pow(1 - k, 3)));
    if (k < 1) requestAnimationFrame(step);
  };
  requestAnimationFrame(step);
}

function setSay(text) {
  const p = $('say');
  if (p.textContent === text) return;
  p.textContent = text;
  const b = $('bubble');
  b.classList.remove('pop');
  void b.offsetWidth;
  b.classList.add('pop');
}

function fillStat(n, limit, showLeft, resetText, inText) {
  const card = $(`st${n}`);
  if (!limit) {
    card.dataset.level = 'x';
    setNum(`n${n}`, null);
    $(`p${n}`).classList.add('off');
    $(`u${n}`).textContent = '';
    $(`b${n}`).style.width = '0';
    $(`r${n}`).textContent = '—';
    $(`i${n}`).textContent = '';
    return;
  }
  const used = Math.round(Math.min(100, Math.max(0, limit.pct)));
  card.dataset.level = String(level(limit.pct));
  setNum(`n${n}`, showLeft ? 100 - used : used);
  $(`p${n}`).classList.remove('off');
  $(`u${n}`).textContent = showLeft ? 'left' : 'used';
  $(`b${n}`).style.width = `${showLeft ? 100 - used : used}%`;
  $(`r${n}`).textContent = resetText;
  $(`i${n}`).textContent = inText;
}

function render() {
  if (!snap) return;
  const s = snap;
  const st = s.settings;
  const t = nowS();
  document.documentElement.dataset.theme = st.theme || 'peach';

  // New session started by Clump → party.
  const p0 = s.pings[0];
  const pingKey = p0 ? `${p0.at}:${p0.ok}` : null;
  if (lastPingSeen !== undefined && pingKey !== lastPingSeen && p0 && p0.ok && t - p0.at < 90) {
    partyUntil = Date.now() + 5000;
    burst($('buddy-btn'));
    setTimeout(render, 5100);
  }
  lastPingSeen = pingKey;

  $('buddy').dataset.mood = moodOf(s, t);
  setSay(sayText(s, t));

  // 5H
  const ss = s.session;
  const active = !!(ss && ss.resets_at && ss.resets_at > t);
  if (active) {
    fillStat(5, ss, st.show_left, hm(ss.resets_at), untilText(ss.resets_at - t));
  } else if (ss && s.status !== 'signed_out') {
    fillStat(5, { pct: 0 }, st.show_left, s.pinging ? 'saying hi' : 'no session', '');
  } else {
    fillStat(5, null);
  }
  // 7D
  const wk = s.weekly;
  if (wk) {
    const r = wk.resets_at;
    fillStat(7, wk, st.show_left, r ? (r - t < 86400 ? hm(r) : `${dow(r)} ${hm(r)}`) : '—', r ? untilText(r - t) : '');
  } else {
    fillStat(7, null);
  }

  // Say hi at reset
  $('auto-session').checked = st.auto_session;
  let sub;
  if (!st.auto_session) sub = 'Off. Your next message starts a new session.';
  else if (s.no_cli) sub = "Can't say hi: install the Claude Code CLI. Clump finds it within a minute.";
  else if (s.pinging) sub = 'Saying hi…';
  else if (active) sub = `Next hi at ${hm(ss.resets_at)}${st.wake_from_sleep ? ' (wakes the PC)' : ''}`;
  else sub = 'Sends hi when the session resets';
  $('auto-sub').textContent = sub;
  $('auto-sub').classList.toggle('bad', st.auto_session && s.no_cli);
  const cl = $('cloud-line');
  cl.hidden = !(st.auto_session && st.cloud_backup);
  if (!cl.hidden) {
    $('cloud-text').textContent = s.cloud_note
      ? `Cloud backup: ${s.cloud_note}`
      : s.cloud_next ? `Cloud hi at ${hm(s.cloud_next)} if this PC is off` : 'Cloud backup waits for the next reset time';
    cl.classList.toggle('bad', !!s.cloud_note);
  }
  const last = $('last');
  if (p0) {
    const when = sameDay(p0.at) ? hm(p0.at) : `${dow(p0.at)} ${hm(p0.at)}`;
    last.textContent = p0.ok ? `Last hi ${when}` : `Last try ${when} failed`;
    last.className = p0.ok ? 'last' : 'last bad';
    last.title = p0.ok ? '' : p0.detail;
  } else {
    last.textContent = 'Nothing sent yet';
    last.className = 'last';
  }
  const start = $('btn-start');
  start.hidden = active || s.status === 'signed_out' || s.demo;
  start.disabled = s.pinging;

  // Footer
  const foot = $('foot');
  const plan = s.plan ? s.plan[0].toUpperCase() + s.plan.slice(1) : '';
  if (s.demo) {
    foot.textContent = 'Demo data';
    foot.className = 'foot';
  } else if (s.status === 'stale') {
    foot.textContent = s.updated_at ? `${s.message}. Numbers from ${hm(s.updated_at)}.` : s.message;
    foot.className = 'foot warn';
  } else {
    foot.textContent = [plan && `${plan} plan`, s.source, s.updated_at && `updated ${agoText(s.updated_at)}`].filter(Boolean).join(' · ');
    foot.className = 'foot';
  }

  // Settings
  for (const b of $('seg-show').children) b.setAttribute('aria-checked', String((b.dataset.v === 'left') === st.show_left));
  for (const b of $('themes').children) b.setAttribute('aria-checked', String(b.dataset.v === (st.theme || 'peach')));
  $('mascot').checked = st.mascot;
  $('show-resets').checked = st.show_resets;
  $('show-widget').checked = st.show_widget;
  $('wake').checked = st.wake_from_sleep;
  $('cloud').checked = st.cloud_backup;
  $('autostart').checked = s.autostart;
  renderRecent(s.pings);
  $('about').textContent = `Clump 1.3 · ${s.source ? `Claude Code in ${s.source}` : 'no Claude Code login found'}`;
  fit();
}

function renderRecent(pings) {
  const box = $('recent');
  box.replaceChildren();
  if (!pings.length) {
    const e = document.createElement('div');
    e.className = 'empty';
    e.textContent = 'Each hi Clump sends is listed here.';
    box.append(e);
    return;
  }
  for (const p of pings.slice(0, 5)) {
    const row = document.createElement('div');
    row.className = 'item';
    const t = document.createElement('span');
    t.className = 't';
    t.textContent = sameDay(p.at) ? hm(p.at) : `${dow(p.at)} ${hm(p.at)}`;
    const d = document.createElement('span');
    d.className = p.ok ? 'd' : 'd bad';
    const how = { manual: ' (manual)', startup: ' at start-up' }[p.trigger] || '';
    d.textContent = p.trigger === 'cloud' ? p.detail : p.ok ? `Session started${how}` : `Failed: ${p.detail}`;
    d.title = p.detail;
    const ms = document.createElement('span');
    ms.className = 'ms';
    ms.textContent = p.ok ? `${(p.ms / 1000).toFixed(1)} s` : '';
    row.append(t, d, ms);
    box.append(row);
  }
}

// ---------- sparkle burst
const BURST_COLORS = ['#ffcf4a', '#f07b5c', '#5ed79e', '#7cc8ff'];
function burst(fromEl) {
  const layer = $('burst');
  const a = fromEl.getBoundingClientRect();
  const b = layer.getBoundingClientRect();
  const x = a.left - b.left + a.width / 2;
  const y = a.top - b.top + a.height / 2;
  for (let i = 0; i < 8; i++) {
    const s = document.createElement('b');
    const ang = (i / 8) * Math.PI * 2 + Math.random() * 0.4;
    const dist = 28 + Math.random() * 26;
    s.style.left = `${x - 6}px`;
    s.style.top = `${y - 6}px`;
    s.style.setProperty('--dx', `${Math.cos(ang) * dist}px`);
    s.style.setProperty('--dy', `${Math.sin(ang) * dist}px`);
    s.style.setProperty('--c', BURST_COLORS[i % BURST_COLORS.length]);
    layer.append(s);
    setTimeout(() => s.remove(), 950);
  }
}

// ---------- window size follows content
let lastHeight = 0;
function fit() {
  requestAnimationFrame(() => {
    const h = Math.ceil($('root').getBoundingClientRect().height);
    if (h > 0 && Math.abs(h - lastHeight) > 1) {
      lastHeight = h;
      invoke('fit_panel', { height: h });
    }
  });
}

function showView(name) {
  for (const v of ['home', 'settings']) {
    const el = $(`view-${v}`);
    const show = v === name;
    if (show && el.hidden) {
      el.classList.remove('slide');
      void el.offsetWidth;
      el.classList.add('slide');
    }
    el.hidden = !show;
  }
  fit();
}

async function patch(p) {
  snap = await invoke('update_settings', { patch: p });
  render();
}

$('auto-session').addEventListener('change', (e) => patch({ auto_session: e.target.checked }));
$('mascot').addEventListener('change', (e) => patch({ mascot: e.target.checked }));
$('show-resets').addEventListener('change', (e) => patch({ show_resets: e.target.checked }));
$('show-widget').addEventListener('change', (e) => patch({ show_widget: e.target.checked }));
$('wake').addEventListener('change', (e) => patch({ wake_from_sleep: e.target.checked }));
$('cloud').addEventListener('change', (e) => patch({ cloud_backup: e.target.checked }));
$('autostart').addEventListener('change', async (e) => { snap = await invoke('set_autostart', { on: e.target.checked }); render(); });
$('seg-show').addEventListener('click', (e) => { const v = e.target.closest('button')?.dataset.v; if (v) patch({ show_left: v === 'left' }); });
$('themes').addEventListener('click', (e) => { const v = e.target.closest('button')?.dataset.v; if (v) patch({ theme: v }); });
$('btn-refresh').addEventListener('click', () => {
  const b = $('btn-refresh');
  b.classList.remove('spin');
  void b.offsetWidth;
  b.classList.add('spin');
  invoke('refresh');
});
$('btn-start').addEventListener('click', () => {
  $('btn-start').disabled = true;
  burst($('btn-start'));
  invoke('start_session');
});
$('buddy-btn').addEventListener('click', () => {
  const hop = $('hop');
  hop.classList.remove('boop');
  void hop.getBoundingClientRect();
  hop.classList.add('boop');
  quip = QUIPS[Math.floor(Math.random() * QUIPS.length)];
  quipUntil = Date.now() + 2600;
  render();
  setTimeout(render, 2700);
});
$('btn-settings').addEventListener('click', () => showView('settings'));
$('btn-back').addEventListener('click', () => showView('home'));
$('btn-quit').addEventListener('click', () => invoke('quit'));
document.addEventListener('keydown', (e) => { if (e.key === 'Escape') invoke('hide_panel'); });
document.addEventListener('contextmenu', (e) => e.preventDefault());

listen('snapshot', (e) => { snap = e.payload; render(); });
listen('panel-shown', () => {
  document.documentElement.classList.remove('asleep');
  showView('home');
  const shell = $('shell');
  shell.classList.remove('enter');
  void shell.offsetWidth;
  shell.classList.add('enter');
  render();
});
listen('panel-hidden', () => document.documentElement.classList.add('asleep'));
document.documentElement.classList.add('asleep');
setInterval(() => { if (!document.documentElement.classList.contains('asleep')) render(); }, 15000);
document.fonts.ready.then(fit);
invoke('get_snapshot').then((s) => { snap = s; render(); });
