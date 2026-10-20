// CaseComplete local UI. No dependencies; talks to /api with the access token.
'use strict';
(() => {
  // ---------- access token ----------
  let TOKEN = '';
  try {
    const q = new URLSearchParams(location.search).get('t');
    if (q) {
      TOKEN = q;
      try { localStorage.setItem('cc.token', q); } catch (_) { /* private mode */ }
    } else {
      TOKEN = localStorage.getItem('cc.token') || '';
    }
  } catch (_) { /* no storage */ }

  const TZ = -new Date().getTimezoneOffset(); // minutes east of UTC
  const DAY = 864e5;
  const REFRESH_MS = 5000;

  class ApiError extends Error {
    constructor(status, message) { super(message); this.status = status; }
  }

  async function api(path, { method = 'GET', body } = {}) {
    const opts = { method, headers: { 'X-Token': TOKEN }, cache: 'no-store' };
    if (body !== undefined) {
      opts.headers['Content-Type'] = 'application/json';
      opts.body = JSON.stringify(body);
    }
    let r;
    try { r = await fetch('/api/' + path, opts); } catch (_) { throw new ApiError(0, 'Cannot reach CaseComplete'); }
    let data = null;
    try { data = await r.json(); } catch (_) { /* empty body */ }
    if (!r.ok) throw new ApiError(r.status, (data && data.error) || r.statusText || 'Request failed');
    return data;
  }

  // ---------- small helpers ----------
  const $app = document.getElementById('app');
  const $toast = document.getElementById('toast');
  const $banner = document.getElementById('banner');

  const esc = (s) => String(s ?? '').replace(/[&<>"']/g, (c) => (
    { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
  const fmt = (n) => Number(n || 0).toLocaleString('en-US');
  const sum = (rows, k) => (rows || []).reduce((a, r) => a + (Number(r[k]) || 0), 0);
  const pct = (a, b) => (b > 0 ? Math.round((100 * a) / b) : 0);
  const clamp = (x, lo, hi) => Math.min(hi, Math.max(lo, x));

  // The Tab-key brand mark: signal-blue keycap on an ink edge, white tab glyph.
  // Sized by CSS (.logo); decorative, the wordmark next to it carries the name.
  const MARK = '<svg class="logo" viewBox="0 0 120 116" aria-hidden="true" focusable="false">'
    + '<rect x="6" y="10" width="108" height="100" rx="22" fill="#17191C"/>'
    + '<rect x="6" y="6" width="108" height="100" rx="22" fill="#3A7BD5"/>'
    + '<path d="M30 56 H78" stroke="#fff" stroke-width="11" stroke-linecap="round"/>'
    + '<path d="M64 38 L84 56 L64 74" fill="none" stroke="#fff" stroke-width="11" stroke-linecap="round" stroke-linejoin="round"/>'
    + '<path d="M92 36 V76" stroke="#fff" stroke-width="11" stroke-linecap="round"/></svg>';

  const APP_NAMES = {
    'code.exe': 'VS Code', 'claude.exe': 'Claude', 'chrome.exe': 'Chrome', 'msedge.exe': 'Edge',
    'firefox.exe': 'Firefox', 'outlook.exe': 'Outlook', 'olk.exe': 'Outlook', 'winword.exe': 'Word',
    'windowsterminal.exe': 'Terminal', 'slack.exe': 'Slack', 'teams.exe': 'Teams',
    'ms-teams.exe': 'Teams', 'discord.exe': 'Discord', 'notepad.exe': 'Notepad',
  };
  function appName(exe) {
    const key = String(exe || '').toLowerCase();
    if (APP_NAMES[key]) return APP_NAMES[key];
    const base = key.replace(/\.exe$/, '').replace(/[-_]+/g, ' ').trim();
    return base ? base.charAt(0).toUpperCase() + base.slice(1) : 'Unknown';
  }

  function hhmm(ts) {
    return new Date(ts).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', hour12: false });
  }
  function when(ts) {
    const d = new Date(ts);
    if (Date.now() - ts < DAY && d.getDate() === new Date().getDate()) return hhmm(ts);
    return d.toLocaleDateString([], { month: 'short', day: 'numeric' }) + ' ' + hhmm(ts);
  }
  function shortDate(iso) {
    const [y, m, d] = String(iso).split('-').map(Number);
    return new Date(y, (m || 1) - 1, d || 1).toLocaleDateString([], { month: 'short', day: 'numeric' });
  }

  function toast(msg) {
    $toast.textContent = msg;
    $toast.classList.add('show');
    clearTimeout(toast.t);
    toast.t = setTimeout(() => $toast.classList.remove('show'), 3200);
  }
  function banner(msg) {
    if (msg) { $banner.textContent = msg; $banner.hidden = false; } else { $banner.hidden = true; }
  }

  // ---------- settings helpers ----------
  const LEVELS = [['off', 'Off'], ['S0', 'Typos'], ['S1', 'Grammar'], ['S2', 'Bold']];
  const LEVEL_NOTES = {
    off: 'Nothing is changed for you.',
    S0: 'Obvious typos only.',
    S1: 'Typos and clear grammar slips.',
    S2: 'Typos and grammar, plus doubled letters in capitalised words such as Outllook.',
  };

  function modelOf(st) {
    const list = (st && st.models) || [];
    return list.find((m) => m.id === st.model) || { id: (st && st.model) || '', label: (st && st.model) || 'No model', origin: '', installed: false };
  }
  const modelLabel = (st) => modelOf(st).label || 'No model';
  function isPaused(st) { return !!(st && st.paused_until_ms && st.paused_until_ms > Date.now()); }
  function statusOf(st) {
    if (!st || !st.enabled) return { cls: 'off', text: 'Off' };
    if (isPaused(st)) return { cls: 'paused', text: 'Paused' };
    return { cls: 'on', text: 'Ready' };
  }
  function profileName(st) {
    const dir = String((st && st.profile_dir) || '').replace(/[\\/]+$/, '');
    return dir.split(/[\\/]/).pop() || 'default';
  }

  // ---------- view state ----------
  const PAGES = ['overview', 'insights', 'context', 'settings', 'words', 'log'];
  const RANGES = [[1, 'Today'], [7, '7 days'], [30, '30 days']];
  const S = {
    route: 'dashboard', page: 'overview', range: 7, logKind: 'all',
    data: {}, key: '', seq: 0, loaded: false, stopped: false, offline: false, dragging: false,
  };
  try {
    const r = Number(localStorage.getItem('cc.range'));
    if ([1, 7, 30].includes(r)) S.range = r;
  } catch (_) { /* ignore */ }

  function parseRoute() {
    const parts = location.hash.replace(/^#\/?/, '').split('/');
    if (parts[0] === 'flyout' || (parts[0] === '' && window.innerWidth <= 440)) {
      return { route: 'flyout', page: '' };
    }
    return { route: 'dashboard', page: PAGES.includes(parts[1]) ? parts[1] : 'overview' };
  }

  // What each view loads, as { key: api path }.
  const sinceMs = () => Date.now() - S.range * DAY;
  const LOADERS = {
    flyout: () => ({
      settings: 'settings',
      series: `series?days=7&tz=${TZ}`,
      learned: 'learned',
      privacy: 'privacy?days=30',
      latency: 'latency?days=7',
    }),
    overview: () => ({
      settings: 'settings',
      series: `series?days=${Math.max(S.range, 7)}&tz=${TZ}`,
      latency: `latency?days=${S.range}`,
      confidence: `confidence?days=${S.range}`,
      events: 'events?limit=6&kind=fix',
      learned: 'learned',
      rules: 'rules',
    }),
    insights: () => ({
      settings: 'settings',
      metrics: `metrics?since_ms=${sinceMs()}`,
      series: `series?days=${Math.max(S.range, 7)}&tz=${TZ}`,
      latency: `latency?days=${S.range}`,
      apps: `apps?days=${S.range}`,
      learned: 'learned',
      privacy: `privacy?days=${S.range}`,
      context: 'context',
      rules: 'rules',
    }),
    context: () => ({ settings: 'settings', context: 'context' }),
    settings: () => ({ settings: 'settings', privacy: 'privacy?days=30' }),
    words: () => ({ settings: 'settings', learned: 'learned' }),
    log: () => ({ settings: 'settings', events: `events?limit=100&kind=${S.logKind}` }),
  };

  async function load() {
    if (S.stopped) return;
    const view = S.route === 'flyout' ? 'flyout' : S.page;
    const key = `${view}|${S.range}|${S.logKind}`;
    const seq = ++S.seq;
    const spec = LOADERS[view]();
    const entries = Object.entries(spec);
    const results = await Promise.allSettled(entries.map(([, p]) => api(p)));
    if (seq !== S.seq || S.stopped) return; // a newer load superseded this one
    if (key !== S.key) { S.data = {}; S.key = key; }
    let failure = null;
    results.forEach((r, i) => {
      if (r.status === 'fulfilled') S.data[entries[i][0]] = r.value;
      else failure = failure || r.reason;
    });
    S.loaded = true;
    if (failure && failure.status === 401) {
      S.stopped = true;
      banner('This page’s access link has expired. Open the dashboard again from the CaseComplete tray menu.');
    } else if (failure && failure.status === 0) {
      S.offline = true;
      banner('Cannot reach CaseComplete. Is it still running?');
    } else {
      S.offline = false;
      banner(failure && failure.status === 503 ? `Some data is unavailable: ${failure.message}` : '');
    }
    render();
  }

  function isEditing() {
    const a = document.activeElement;
    if (!a || !$app.contains(a)) return false;
    return (a.tagName === 'INPUT' && !['button', 'checkbox'].includes(a.type)) || a.tagName === 'TEXTAREA';
  }

  function tick() {
    if (S.stopped || document.hidden || isEditing() || S.dragging) return;
    load();
  }

  // ---------- charts ----------
  function areaChart(rows, label) {
    const W = 700, H = 150;
    const pts = [];
    rows.forEach((r, i) => { if (r.shown > 0) pts.push({ i, r: r.accepted / r.shown }); });
    if (!pts.length) return '<div class="empty">No suggestions shown in this period yet.</div>';
    const n = rows.length;
    const max = Math.max(0.1, ...pts.map((p) => p.r)) * 1.2;
    const x = (i) => (n > 1 ? (i * W) / (n - 1) : W / 2);
    const y = (r) => H - 12 - (r / max) * (H - 34);
    const c = pts.map((p) => [x(p.i), y(p.r)]);
    const line = c.map((p, k) => `${k ? 'L' : 'M'}${p[0].toFixed(1)} ${p[1].toFixed(1)}`).join(' ');
    const area = `${line} L${c[c.length - 1][0].toFixed(1)} ${H} L${c[0][0].toFixed(1)} ${H} Z`;
    const grid = [0.25, 0.5, 0.75].map((f) => `<line x1="0" x2="${W}" y1="${H * f}" y2="${H * f}" stroke="var(--line)" stroke-width="1" vector-effect="non-scaling-stroke"/>`).join('');
    const last = pts[pts.length - 1], first = pts[0];
    const ticks = [0, Math.round((n - 1) / 3), Math.round(((n - 1) * 2) / 3), n - 1]
      .filter((v, k, a) => a.indexOf(v) === k)
      .map((i) => `<span>${esc(shortDate(rows[i].date))}</span>`).join('');
    return `<div class="area">
      <svg viewBox="0 0 ${W} ${H}" preserveAspectRatio="none" role="img" aria-label="${esc(label)}">${grid}
        <path d="${area}" fill="var(--accent)" opacity="0.14"/>
        <path d="${line}" fill="none" stroke="var(--accent)" stroke-width="2.5" stroke-linejoin="round" vector-effect="non-scaling-stroke"/>
      </svg>
      <span class="dot-end" style="left:${((x(last.i) / W) * 100).toFixed(2)}%;top:${((y(last.r) / H) * 100).toFixed(2)}%"></span>
      <span class="badge">${Math.round(last.r * 100)}%</span>
      ${pts.length > 1 ? `<span class="tag" style="top:${((y(first.r) / H) * 100).toFixed(2)}%">${Math.round(first.r * 100)}%</span>` : ''}
    </div><div class="axis">${ticks}</div>`;
  }

  const bucketOf = (ms) => clamp(Math.floor((ms || 0) / 20), 0, 15);

  function histogram(l) {
    const b = (l && l.buckets) || [];
    if (!l || !l.total) return '<div class="empty">No suggestions timed yet.</div>';
    const max = Math.max(1, ...b.map((x) => x.count));
    const lo = bucketOf(l.p50_ms), hi = bucketOf(l.p95_ms);
    const bars = b.map((x, i) => {
      const range = x.to === null ? `${x.from} ms and up` : `${x.from}–${x.to} ms`;
      return `<i class="${i >= lo && i <= hi ? 'hi' : ''}" style="height:${((x.count / max) * 100).toFixed(1)}%" title="${range}: ${x.count}"></i>`;
    }).join('');
    return `<div class="hist" role="img" aria-label="Latency histogram, median ${l.p50_ms} ms, 95th percentile ${l.p95_ms} ms">${bars}</div>
      <div class="axis"><span>0 ms</span><span>100</span><span>200</span><span>300+</span></div>`;
  }

  function appBars(apps) {
    const rows = (apps || []).filter((a) => a.fixes_kept + a.accepted + a.fixes_reverted + a.shown > 0).slice(0, 6);
    if (!rows.length) return '<div class="empty">No activity in this period yet.</div>';
    const max = Math.max(1, ...rows.map((a) => a.fixes_kept + a.accepted));
    let anyWarn = false;
    const cells = rows.map((a) => {
      const val = a.fixes_kept + a.accepted;
      const fixes = a.fixes_kept + a.fixes_reverted;
      const warn = fixes >= 4 && a.fixes_reverted / fixes > 0.25;
      anyWarn = anyWarn || warn;
      return `<span class="name" title="${esc(a.app_exe)}">${esc(appName(a.app_exe))}</span>
        <span class="track"><i class="${warn ? 'warn' : ''}" style="width:${Math.max(2, (val / max) * 100).toFixed(1)}%"></i></span>
        <span class="count ${warn ? 'warn' : ''}">${fmt(val)}</span>`;
    }).join('');
    const off = (apps || []).filter((a) => a.autofix_off).map((a) =>
      `<span class="note">Auto-fix is off in ${esc(appName(a.app_exe))}: ${fmt(a.fixes_misplaced)} of its last ${fmt(a.fixes_checked)} fixes did not land as typed. It comes back a week after those misses.</span>`).join('');
    return `<div class="apps">${cells}</div>${anyWarn ? '<span class="note">Orange: an app where fixes get undone more than 25% of the time.</span>' : ''}${off}`;
  }

  function confidenceChart(rows, floor) {
    const total = sum(rows, 'shown');
    if (!total) return '<div class="empty">No suggestions in this period yet. Bars will show how often you take them at each confidence.</div>';
    const max = Math.max(1, ...rows.map((r) => r.shown));
    const bars = rows.map((r) => {
      const h = Math.max(1.5, (r.shown / max) * 100);
      const inner = r.shown ? (r.accepted / r.shown) * 100 : 0;
      return `<div class="cb" title="${r.lo.toFixed(1)}–${r.hi.toFixed(1)}: ${r.accepted} of ${r.shown} taken"><div class="cbar" style="height:${h.toFixed(1)}%"><i style="height:${inner.toFixed(1)}%"></i></div></div>`;
    }).join('');
    const axis = rows.map((r) => `<span>${r.lo.toFixed(1)}</span>`).join('');
    return `<div class="conf"><div class="conf-bars" role="img" aria-label="Suggestions shown and accepted by confidence">${bars}<span class="conf-marker" data-marker style="left:${(clamp(floor, 0, 1) * 100).toFixed(1)}%"></span></div>
      <div class="conf-axis">${axis}</div></div>
      <div class="legend"><span><i></i>shown</span><span><i class="a"></i>taken</span><span><span style="color:var(--warn)">- - -</span> your floor</span></div>`;
  }

  function thresholdRow(st) {
    const v = st && typeof st.min_confidence === 'number' ? st.min_confidence : 0;
    return `<div class="thresh"><span>Show suggestions from confidence</span>
      <div class="ctl"><input type="range" min="0" max="1" step="0.01" value="${v}" data-input="conf" data-change="conf" aria-label="Minimum confidence">
      <span class="val" data-conf-val>${v.toFixed(2)}</span></div></div>`;
  }

  // ---------- shared view pieces ----------
  const card = (cls, title, right, body) => `<section class="card ${cls}"><div class="hd"><b>${title}</b>${right ? `<span>${right}</span>` : ''}</div>${body}</section>`;

  function outcomeCell(o) {
    const map = {
      kept: ['kept', 'good'], accepted: ['accepted', 'good'], reverted: ['undone', 'warn'],
      overwritten: ['edited', 'warn'], ignored: ['ignored', 'dim'], timeout: ['expired', 'dim'],
    };
    const [text, cls] = map[o] || ['pending', 'dim'];
    return `<span class="${cls}">${text}</span>`;
  }

  function eventText(e) {
    if (e.source === 'llm_complete') return `<span class="muted">→</span> ${esc(e.replacement)}`;
    return `<s>${esc(e.original)}</s> → ${esc(e.replacement)}`;
  }

  function rangeSeg() {
    return `<div class="seg small" role="group" aria-label="Time range">${RANGES.map(([d, l]) => `<button data-act="range" data-v="${d}" aria-pressed="${S.range === d}">${l}</button>`).join('')}</div>`;
  }

  function sidebar(st) {
    const s = statusOf(st);
    const items = [['overview', 'Overview'], ['insights', 'Insights'], ['context', 'Context'], ['settings', 'Settings'], ['words', 'Your words'], ['log', 'Log']];
    return `<nav class="side" aria-label="Sections">
      <div class="brand">${MARK}<span class="wm"><span>CaseComplete</span>${S.page === 'overview' ? '<span class="tagline">Just type.</span>' : ''}</span></div>
      ${items.map(([k, l]) => `<a href="#/dashboard${k === 'overview' ? '' : '/' + k}" class="${S.page === k ? 'on' : ''}"${S.page === k ? ' aria-current="page"' : ''}>${l}</a>`).join('')}
      <div class="side-foot"><span class="stat"><i class="dot ${s.cls}"></i>${s.cls === 'on' ? 'Running' : s.text} · ${esc(modelLabel(st))}</span>
      <span>Profile: ${esc(profileName(st))} · all local</span></div></nav>`;
  }

  // ---------- flyout ----------
  function viewFlyout(d) {
    const st = d.settings || {};
    const rows = d.series || [];
    const today = rows[rows.length - 1] || {};
    const week = rows.slice(-7);
    const s = statusOf(st);
    const rates = week.map((r) => (r.shown > 0 ? r.accepted / r.shown : 0));
    const top = Math.max(0.01, ...rates);
    const bars = week.map((r, i) => {
      const h = Math.max(6, (rates[i] / top) * 100);
      return `<i class="${i === week.length - 1 ? 'hi' : ''}" style="height:${h.toFixed(0)}%" title="${esc(shortDate(r.date))}: ${r.accepted} of ${r.shown} accepted"></i>`;
    }).join('');
    let line = st.status_line;
    if (!line) {
      const lat = d.latency && d.latency.total ? ` · ${d.latency.p50_ms} ms` : '';
      line = `${s.text} · ${modelLabel(st)}${lat}`;
    }
    const learned = d.learned ? (d.learned.autocorrect_pairs || 0) + (d.learned.dictionary_words || 0) : 0;
    const off = d.privacy ? d.privacy.apps_kept_off || 0 : 0;
    const paused = isPaused(st);
    return `<main class="fly">
      <div class="f-head">${MARK}
        <div class="who"><span class="name">CaseComplete</span><span class="line" title="${esc(line)}"><i class="dot ${s.cls}"></i>${esc(line)}</span></div>
        <button class="sw" role="switch" aria-checked="${!!st.enabled}" aria-label="Enabled" data-act="enabled"></button></div>
      <div class="f-stats">
        <div class="f-box"><span class="n">${fmt(today.fixes_kept)}</span><span class="l">fixes kept</span></div>
        <div class="f-box"><span class="n">${fmt(today.accepted)}</span><span class="l">sentences</span></div>
        <div class="f-box"><span class="n">${fmt(today.chars_saved)}</span><span class="l">chars saved</span></div></div>
      <div class="f-box f-week"><div class="t"><span>Accepted suggestions, last 7 days</span><span>${pct(sum(week, 'accepted'), sum(week, 'shown'))}%</span></div>
        <div class="bars" role="img" aria-label="Acceptance rate for each of the last 7 days">${bars}</div></div>
      <div class="f-auto"><span class="f-label">Auto-fix</span>
        <div class="seg" role="group" aria-label="Auto-fix level">${LEVELS.map(([v, l]) => `<button data-act="level" data-v="${v}" aria-pressed="${st.auto_fix_level === v}">${l}</button>`).join('')}</div></div>
      <div class="f-rows">
        <button type="button" data-act="dashboard" data-v="settings"><span>Model</span><span>${esc(modelLabel(st))} ›</span></button>
        <button type="button" data-act="dashboard" data-v="settings"><span>Privacy</span><span>Off in ${off} app${off === 1 ? '' : 's'} ›</span></button>
        <button type="button" data-act="dashboard" data-v="words"><span>Your words</span><span>${fmt(learned)} learned ›</span></button></div>
      <div class="f-foot"><button class="btn" data-act="pause">${paused ? 'Resume' : 'Pause 10 min'}</button>
        <div class="links"><button type="button" class="link" data-act="dashboard" data-v="">Dashboard</button><button type="button" class="link" data-act="dashboard" data-v="log">Log</button><button class="quit" data-act="quit">Quit</button></div></div>
    </main>`;
  }

  // ---------- dashboard pages ----------
  function pageOverview(d) {
    const st = d.settings || {};
    const rows = (d.series || []).slice(-S.range);
    const kept = sum(rows, 'fixes_kept'), undone = sum(rows, 'fixes_reverted');
    const shown = sum(rows, 'shown'), acc = sum(rows, 'accepted'), chars = sum(rows, 'chars_saved');
    const l = d.latency || {};
    const mins = Math.round(chars / 270);
    const recent = (d.events || []).slice(0, 5);
    const demoted = (d.rules || []).filter((r) => r.demoted);
    const ln = d.learned || {};
    const learnedLines = [
      ...demoted.map((r) => `Rule <b>${esc(r.rule)}</b> is suggest-only after ${r.auto_reverted} undo${r.auto_reverted === 1 ? '' : 's'}.`),
      ln.dictionary_words ? `<b>${fmt(ln.dictionary_words)}</b> word${ln.dictionary_words === 1 ? '' : 's'} in your dictionary.` : '',
      ln.autocorrect_pairs ? `<b>${fmt(ln.autocorrect_pairs)}</b> autocorrect pair${ln.autocorrect_pairs === 1 ? '' : 's'} learned from you.` : '',
    ].filter(Boolean);
    return `<div class="title"><h1>Overview</h1>${rangeSeg()}</div>
      <section class="card kpi s3"><span class="l">Fixes kept</span><span class="v">${fmt(kept)}</span>
        <span class="n ${kept + undone ? 'good' : ''}">${kept + undone ? `${pct(kept, kept + undone)}% kept · ${fmt(undone)} undone` : 'no fixes yet'}</span></section>
      <section class="card kpi s3"><span class="l">Sentences accepted</span><span class="v">${fmt(acc)}</span>
        <span class="n">${shown ? `of ${fmt(shown)} shown · ${pct(acc, shown)}%` : 'none shown yet'}</span></section>
      <section class="card kpi s3"><span class="l">Characters saved</span><span class="v">${fmt(chars)}</span>
        <span class="n">${chars ? (mins >= 1 ? `about ${fmt(mins)} minute${mins === 1 ? '' : 's'} of typing` : 'under a minute of typing') : 'nothing saved yet'}</span></section>
      <section class="card kpi s3"><span class="l">Keystroke to ghost</span><span class="v">${l.total ? `${fmt(l.p50_ms)}<small>ms</small>` : '–'}</span>
        <span class="n">${l.total ? `p95 ${fmt(l.p95_ms)} ms` : 'no suggestions timed yet'}</span></section>
      ${card('s7', 'Acceptance by confidence', 'suggestions shown at each confidence, and how many you took',
        confidenceChart(d.confidence || [], st.min_confidence || 0) + thresholdRow(st))}
      ${card('s5', 'Recent fixes', '<a href="#/dashboard/log">open log</a>',
        `${recent.length ? `<div class="recent">${recent.map((e) => `<div class="r"><span>${hhmm(e.ts)}</span><span title="${esc(appName(e.app_exe))}">${eventText(e)}</span>${outcomeCell(e.outcome)}</div>`).join('')}</div>` : '<div class="empty">Fixes will show up here as you type.</div>'}
        <div class="learned"><b>Learned</b>${learnedLines.length ? learnedLines.map((x) => `<span>${x}</span>`).join('') : '<span>Nothing yet. It learns from what you keep and undo.</span>'}</div>`)}`;
  }

  function pageInsights(d) {
    const st = d.settings || {};
    const m = d.metrics || {};
    const rows = d.series || [];
    const l = d.latency || {};
    const ln = d.learned || {};
    const pv = d.privacy || {};
    const br = pv.by_reason || {};
    const known = ['card number', 'long number', 'secret after its label'];
    const otherBlocked = Object.entries(br).filter(([k]) => !known.includes(k) && !/^(app |title)/i.test(k)).reduce((a, [, v]) => a + v, 0);
    const demoted = (d.rules || []).filter((r) => r.demoted).length;
    const pairs = (ln.autocorrect || []).slice(0, 3).map((p) => `<span class="chip">${esc(p.typo)} → ${esc(p.fix)}</span>`);
    const words = (ln.dictionary || []).slice(0, 2).map((w) => `<span class="chip">${esc(w)} ✓</span>`);
    const total = (ln.autocorrect || []).length + (ln.dictionary || []).length;
    const shownChips = pairs.length + words.length;
    const rangeName = RANGES.find(([v]) => v === S.range)[1].toLowerCase();
    const ctx = d.context;
    return `<div class="title"><h1>Insights<small>${rangeName} · ${fmt(m.events)} events · all local</small></h1>${rangeSeg()}</div>
      ${card('s7', 'Acceptance over time', 'sentences accepted ÷ shown, per day',
        areaChart(rows, 'Acceptance rate per day'))}
      ${card('s5', 'Keystroke to ghost', l.total ? `p50 ${fmt(l.p50_ms)} · p95 ${fmt(l.p95_ms)} ms` : '', histogram(l))}
      ${card('s5', 'By app', 'fixes kept · sentences accepted', appBars(d.apps))}
      ${card('s4', 'What it learned', 'so far', `<div class="kv">
          <div><span>Autocorrect pairs learned</span><span class="mono">${fmt(ln.autocorrect_pairs)}</span></div>
          <div><span>Words added to dictionary</span><span class="mono">${fmt(ln.dictionary_words)}</span></div>
          <div><span>Rules demoted</span><span class="mono">${fmt(demoted)}</span></div>
          <div><span>Confidence floor</span><span class="mono">${(st.min_confidence || 0).toFixed(2)}</span></div></div>
        <div class="chips">${pairs.join('')}${words.join('')}${total > shownChips ? `<span class="chip"><a href="#/dashboard/words">+${total - shownChips} more</a></span>` : ''}</div>`)}
      ${card('s3', 'Privacy gate', rangeName, `<div class="kv">
          <div><span>Sentences never sent</span><span class="mono">${fmt(pv.sentences_blocked)}</span></div>
          <div><span>Card numbers</span><span class="mono">${fmt(br['card number'])}</span></div>
          <div><span>Long numbers</span><span class="mono">${fmt(br['long number'])}</span></div>
          <div><span>Labelled secrets</span><span class="mono">${fmt(br['secret after its label'])}</span></div>
          ${otherBlocked ? `<div><span>Other</span><span class="mono">${fmt(otherBlocked)}</span></div>` : ''}
          <div><span>Apps kept off</span><span class="mono">${fmt(pv.apps_kept_off)}</span></div></div>
        <span class="note good" style="margin-top:auto">Nothing left this machine.</span>`)}
      ${card('s7', 'Context: what the model saw for the last suggestion', ctx ? `${esc(appName(ctx.app_exe))} · ${new Date(ctx.ts_ms).toLocaleTimeString()} · ${ctx.latency_ms} ms` : '', contextBody(ctx, st))}
      ${card('s5', 'Settings', '<a href="#/dashboard/settings">all settings</a>', `<div class="cells">
          <div class="cell"><span class="k">Model</span><span class="x">${esc(modelLabel(st))}${modelOf(st).origin ? ' · ' + esc(modelOf(st).origin) : ''}</span></div>
          <div class="cell"><span class="k">Auto-fix</span><span class="x">${esc(levelText(st.auto_fix_level))}</span></div>
          <div class="cell"><span class="k">Confidence floor</span><span class="x">${(st.min_confidence || 0).toFixed(2)}</span></div>
          <div class="cell"><span class="k">Status</span><span class="x">${esc(statusOf(st).text)}</span></div>
          <div class="cell"><span class="k">Start with Windows</span><span class="x ${st.autostart ? 'good' : ''}">${st.autostart ? 'On' : 'Off'}</span></div>
          <div class="cell"><span class="k">Profile</span><span class="x">${esc(profileName(st))}</span></div></div>`)}`;
  }

  function levelText(level) {
    const f = LEVELS.find(([v]) => v === level);
    return f ? `${f[1]}${level === 'off' ? '' : ` (${level})`}` : String(level || '');
  }

  function contextBody(c, st) {
    if (!c) return '<div class="empty">Nothing yet. The last suggestion appears here once you type somewhere.</div>';
    const style = c.style_lines || [];
    const floor = st && typeof st.min_confidence === 'number' ? st.min_confidence : null;
    const gate = c.privacy_block ? 'blocked' : (floor === null ? '–' : (c.confidence >= floor ? `passed ${floor.toFixed(2)}` : `below ${floor.toFixed(2)}`));
    // The prompt box is pre-wrapped, so its markup must not contain layout whitespace.
    const prompt = [
      `<span class="lab">style prefix (${style.length} of your accepted sentence${style.length === 1 ? '' : 's'})</span>`,
      style.length ? style.map((x) => esc(x)).join('\n') : '<span class="dim">none yet</span>',
      '<span class="lab gap">typed text (last 800 chars, trailing space trimmed)</span>',
      c.prompt_tail ? esc(c.prompt_tail) : '<span class="dim">hidden or empty</span>',
      '<span class="out gap">→ ',
      c.completion ? `<em>${esc(c.completion)}</em>` : '<span class="dim">no suggestion</span>',
      ` <span class="dim">conf ${Number(c.confidence).toFixed(2)} · ${c.tokens} token${c.tokens === 1 ? '' : 's'}${c.outcome ? ' · ' + esc(c.outcome) : ''}</span></span>`,
    ].join('');
    return `<div class="ctx-grid"><div class="prompt">${prompt}</div>
      <div class="kv">
        <div><span class="muted">App</span><span>${esc(appName(c.app_exe))}</span></div>
        <div><span class="muted">Healing</span><span>${c.heal ? `“${esc(c.heal)}”` : 'off'}</span></div>
        <div><span class="muted">Leading space</span><span>${c.need_space ? 'added by the model' : 'not needed'}</span></div>
        <div><span class="muted">Prompt</span><span>${Number(c.prompt_ms).toFixed(0)} ms</span></div>
        <div><span class="muted">Gate</span><span>${esc(gate)}</span></div>
        <div><span class="muted">Privacy</span><span class="${c.privacy_block ? 'warn' : 'good'}">${c.privacy_block ? esc(c.privacy_block) : 'clean'}</span></div></div></div>`;
  }

  function pageContext(d) {
    return `<div class="title"><h1>Context<small>what the model saw for the last suggestion</small></h1></div>
      ${card('s12', 'Last suggestion', d.context ? `${esc(appName(d.context.app_exe))} · ${new Date(d.context.ts_ms).toLocaleTimeString()} · ${d.context.latency_ms} ms` : '', contextBody(d.context, d.settings))}
      <p class="note s12" style="grid-column:span 12">Everything here stays on this computer. Text near passwords, card numbers and the apps on your privacy list is never sent to the model.</p>`;
  }

  function pageSettings(d) {
    const st = d.settings || {};
    const pv = d.privacy || {};
    const paused = isPaused(st);
    const models = (st.models || []).map((m) => `<button class="model" data-act="model" data-v="${esc(m.id)}" aria-pressed="${st.model === m.id}">
        <span class="m"><span>${esc(m.label)}</span><small>${esc(m.origin)}</small></span>
        <span class="tag ${m.installed ? 'ok' : ''}">${m.installed ? 'installed' : 'not installed'}</span></button>`).join('');
    return `<div class="title"><h1>Settings</h1></div>
      ${card('s6', 'Status', paused ? `paused until ${hhmm(st.paused_until_ms)}` : '', `
        <div class="row-line"><div class="t"><span>CaseComplete is ${st.enabled ? 'on' : 'off'}</span><small>${esc(st.status_line || '')}</small></div>
          <button class="sw" role="switch" aria-checked="${!!st.enabled}" aria-label="Enabled" data-act="enabled"></button></div>
        <div class="btns">${paused ? '<button class="btn" data-act="resume">Resume now</button>' : [10, 30, 60].map((m) => `<button class="btn" data-act="pause" data-v="${m}">Pause ${m} min</button>`).join('')}</div>`)}
      ${card('s6', 'Auto-fix', '', `<div class="seg" role="group" aria-label="Auto-fix level">${LEVELS.map(([v, l]) => `<button data-act="level" data-v="${v}" aria-pressed="${st.auto_fix_level === v}">${l}</button>`).join('')}</div>
        <span class="note">${esc(LEVEL_NOTES[st.auto_fix_level] || '')}</span>`)}
      ${card('s6', 'Model', 'runs on this computer', st.models && st.models.length ? `<div class="models">${models}</div>` : `<div class="empty">The app has not reported any models. Current: ${esc(st.model || 'none')}.</div>`)}
      ${card('s6', 'Confidence floor', 'hide suggestions the model is unsure about',
        `${thresholdRow(st)}<span class="note">Lower shows more suggestions, higher shows fewer. The Overview chart shows how often you take them at each level.</span>`)}
      ${card('s6', 'General', '', `
        <div class="row-line"><div class="t"><span>Start with Windows</span><small>Launch CaseComplete when you sign in.</small></div>
          <button class="sw" role="switch" aria-checked="${!!st.autostart}" aria-label="Start with Windows" data-act="autostart"></button></div>
        <div class="row-line"><div class="t"><span>Profile folder</span><small>${esc(st.profile_dir || '')}</small></div>
          <button class="btn" data-act="open-profile">Open</button></div>`)}
      ${card('s6', 'Privacy', 'where the assistant stays off', `
        ${pv.policy_present === false ? '<span class="note">No privacy.toml found in the profile yet.</span>' : ''}
        <span class="note">Apps</span><div class="chips">${(pv.deny_exe || []).map((x) => `<span class="chip">${esc(x)}</span>`).join('') || '<span class="dim">none</span>'}</div>
        <span class="note">Window titles containing</span><div class="chips">${(pv.deny_title || []).map((x) => `<span class="chip">${esc(x)}</span>`).join('') || '<span class="dim">none</span>'}</div>
        <span class="note">Edit <span class="mono">privacy.toml</span> in the profile folder to change these. Text that looks like a card number, SSN or password is never sent.</span>`)}
      <div class="s12" style="grid-column:span 12"><button class="btn danger" data-act="quit">Quit CaseComplete</button></div>`;
  }

  function pageWords(d) {
    const ln = d.learned || {};
    const dict = ln.dictionary || [], pairs = ln.autocorrect || [];
    return `<div class="title"><h1>Your words<small>${fmt(dict.length)} in dictionary · ${fmt(pairs.length)} autocorrect pairs</small></h1></div>
      ${card('s6', 'Dictionary', 'words that are never corrected', `
        <form class="add" data-form="word-add" autocomplete="off"><input type="text" name="word" placeholder="Add a word" maxlength="64" aria-label="Word to add"><button class="btn primary" type="submit">Add</button></form>
        ${dict.length ? `<div class="chips">${dict.map((w) => `<span class="chip">${esc(w)}<button data-act="word-del" data-v="${esc(w)}" aria-label="Remove ${esc(w)}">×</button></span>`).join('')}</div>` : '<div class="empty">No words yet. Words you keep after undoing a fix land here.</div>'}`)}
      ${card('s6', 'Autocorrect', 'typo → fix, learned from you', `
        <form class="add" data-form="ac-add" autocomplete="off"><input type="text" name="typo" placeholder="Typo" maxlength="64" aria-label="Typo"><input type="text" name="fix" placeholder="Fix" maxlength="128" aria-label="Fix"><button class="btn primary" type="submit">Add</button></form>
        ${pairs.length ? `<div class="pairs">${pairs.map((p) => `<div class="p"><span>${esc(p.typo)} <span class="muted">→</span> ${esc(p.fix)}</span><button data-act="ac-del" data-v="${esc(p.typo)}" aria-label="Remove ${esc(p.typo)}">×</button></div>`).join('')}</div>` : '<div class="empty">No pairs yet. Built-in fixes are not listed here.</div>'}`)}`;
  }

  function pageLog(d) {
    const rows = d.events || [];
    const kinds = [['all', 'All'], ['fix', 'Fixes'], ['complete', 'Suggestions']];
    return `<div class="title"><h1>Log<small>newest first · last ${rows.length}</small></h1>
        <div class="seg small" role="group" aria-label="Event type">${kinds.map(([v, l]) => `<button data-act="log-kind" data-v="${v}" aria-pressed="${S.logKind === v}">${l}</button>`).join('')}</div></div>
      <section class="card s12">${rows.length ? `<div class="tbl">
        <div class="tr th"><span>When</span><span>App</span><span>Type</span><span>What</span><span class="num">Conf</span><span class="num">ms</span><span>Outcome</span></div>
        ${rows.map((e) => `<div class="tr"><span class="muted">${esc(when(e.ts))}</span><span title="${esc(e.app_exe)}">${esc(appName(e.app_exe))}</span>
          <span class="muted" title="${esc(e.source)} · ${esc(e.action)}">${e.source === 'llm_complete' ? 'suggestion' : esc(e.rule)}</span><span title="${esc(e.original)} → ${esc(e.replacement)}">${eventText(e)}</span>
          <span class="num">${Number(e.confidence).toFixed(2)}</span><span class="num">${e.latency_ms}</span>${outcomeCell(e.outcome)}</div>`).join('')}</div>` : '<div class="empty">Nothing logged yet.</div>'}</section>`;
  }

  const PAGE_VIEWS = { overview: pageOverview, insights: pageInsights, context: pageContext, settings: pageSettings, words: pageWords, log: pageLog };

  // ---------- render ----------
  function render() {
    if (S.stopped && S.quit) return;
    document.body.className = S.route === 'flyout' ? 'light' : 'dark';
    document.title = S.route === 'flyout' ? 'CaseComplete' : `CaseComplete · ${S.page}`;
    if (!S.loaded) { $app.innerHTML = ''; return; }
    const d = S.data;
    const typing = captureInput();
    if (S.route === 'flyout') {
      $app.innerHTML = viewFlyout(d);
    } else {
      $app.innerHTML = `<div class="dash">${sidebar(d.settings)}<main class="main">${PAGE_VIEWS[S.page](d)}</main></div>`;
    }
    restoreInput(typing);
  }

  // Re-rendering replaces the DOM; keep a word being typed (and the caret) across it.
  function captureInput() {
    const a = document.activeElement;
    const form = a && a.closest ? a.closest('[data-form]') : null;
    if (!form || a.tagName !== 'INPUT') return null;
    return { form: form.dataset.form, name: a.name, value: a.value, start: a.selectionStart, end: a.selectionEnd };
  }

  function restoreInput(t) {
    if (!t) return;
    const el = $app.querySelector(`[data-form="${t.form}"] input[name="${t.name}"]`);
    if (!el) return;
    el.value = t.value;
    el.focus();
    try { el.setSelectionRange(t.start, t.end); } catch (_) { /* not supported for this type */ }
  }

  // ---------- actions ----------
  async function post(path, body, method = 'POST') {
    try {
      return await api(path, { method, body });
    } catch (e) {
      toast(e.message);
      throw e;
    }
  }

  async function setSettings(patch) {
    try {
      S.data.settings = await post('settings', patch);
      render();
    } catch (_) { load(); }
  }

  const ACT = {
    enabled: () => setSettings({ enabled: !(S.data.settings || {}).enabled }),
    level: (el) => setSettings({ auto_fix_level: el.dataset.v }),
    pause: (el) => {
      if (S.route === 'flyout' && isPaused(S.data.settings)) return setSettings({ enabled: true });
      return setSettings({ pause_minutes: Number(el.dataset.v || 10) });
    },
    resume: () => setSettings({ enabled: true }),
    model: (el) => setSettings({ model: el.dataset.v }),
    autostart: () => setSettings({ autostart: !(S.data.settings || {}).autostart }),
    range: (el) => {
      S.range = Number(el.dataset.v);
      try { localStorage.setItem('cc.range', String(S.range)); } catch (_) { /* ignore */ }
      load();
    },
    'log-kind': (el) => { S.logKind = el.dataset.v; load(); },
    // The flyout lives in its own WebView2 window, so the dashboard opens through the app.
    dashboard: async (el) => {
      try { await post('open-dashboard', { page: el.dataset.v || '' }); } catch (_) { /* toasted */ }
    },
    'open-profile': async () => {
      try { await post('open-profile'); toast('Opening the profile folder…'); } catch (_) { /* toasted */ }
    },
    'word-del': async (el) => { await afterWordEdit(post('words', { remove: el.dataset.v })); },
    'ac-del': async (el) => { await afterWordEdit(post('autocorrect', { typo: el.dataset.v }, 'DELETE')); },
    quit: async () => {
      if (!window.confirm('Quit CaseComplete? Typing help stops until you start it again.')) return;
      try { await post('quit'); } catch (_) { return; }
      S.stopped = true; S.quit = true;
      $app.innerHTML = '<div class="goodbye"><div><h1>CaseComplete has quit</h1><p>You can close this tab.</p></div></div>';
    },
  };

  // The orchestrator applies word edits asynchronously and rewrites the files; look again soon.
  async function afterWordEdit(promise) {
    try { await promise; } catch (_) { return; }
    setTimeout(load, 350);
    setTimeout(load, 1500);
  }

  const FORMS = {
    'word-add': async (form) => {
      const word = form.elements.word.value.trim();
      if (!word) return;
      try { await post('words', { add: word }); } catch (_) { return; }
      form.reset();
      toast(`Added “${word}”`);
      setTimeout(load, 350); setTimeout(load, 1500);
    },
    'ac-add': async (form) => {
      const typo = form.elements.typo.value.trim(), fix = form.elements.fix.value.trim();
      if (!typo || !fix) { toast('Enter both the typo and its fix'); return; }
      try { await post('autocorrect', { typo, fix }); } catch (_) { return; }
      form.reset();
      toast(`Added ${typo} → ${fix}`);
      setTimeout(load, 350); setTimeout(load, 1500);
    },
  };

  $app.addEventListener('click', (e) => {
    const el = e.target.closest('[data-act]');
    if (!el || !ACT[el.dataset.act]) return;
    e.preventDefault();
    ACT[el.dataset.act](el);
  });

  $app.addEventListener('submit', (e) => {
    const form = e.target.closest('[data-form]');
    if (!form) return;
    e.preventDefault();
    FORMS[form.dataset.form](form);
  });

  // Confidence slider: live label and marker while dragging, one POST when released.
  $app.addEventListener('input', (e) => {
    const el = e.target.closest('[data-input="conf"]');
    if (!el) return;
    S.dragging = true;
    const v = Number(el.value);
    $app.querySelectorAll('[data-conf-val]').forEach((n) => { n.textContent = v.toFixed(2); });
    $app.querySelectorAll('[data-marker]').forEach((n) => { n.style.left = `${(v * 100).toFixed(1)}%`; });
  });
  $app.addEventListener('change', async (e) => {
    const el = e.target.closest('[data-change="conf"]');
    if (!el) return;
    const v = Math.round(Number(el.value) * 100) / 100;
    await setSettings({ min_confidence: v });
    S.dragging = false;
    document.activeElement && document.activeElement.blur && document.activeElement.blur();
  });
  document.addEventListener('pointerup', () => { if (S.dragging) setTimeout(() => { S.dragging = false; }, 400); });

  // ---------- boot ----------
  function route() {
    const r = parseRoute();
    S.route = r.route;
    S.page = r.page || S.page;
    S.loaded = false;
    S.key = '';
    render();
    load();
  }
  window.addEventListener('hashchange', route);
  document.addEventListener('visibilitychange', () => { if (!document.hidden) tick(); });
  setInterval(tick, REFRESH_MS);
  route();
})();
