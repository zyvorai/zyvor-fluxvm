// FluxVM Cloud console. A tenant leases Macs, launches VMs from the image catalog, runs commands in them, connects
// them with private networks and sees its usage; the operator also runs the Macs (cordon, drain, scrub), tenants,
// the catalog and the policy. Talks only to this central's /fleet API. Everything from the API is rendered with
// textContent, never as HTML.
'use strict';

// ---- small helpers --------------------------------------------------------------------------------------------------

const TOKEN_KEY = 'fluxvm.fleet.token';
const THEME_KEY = 'fluxvm.console.theme';
const $ = (id) => document.getElementById(id);

function h(tag, attrs, ...children) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === undefined || v === null || v === false) continue;
    if (k === 'class') e.className = v;
    else if (k === 'style' && typeof v === 'object') Object.assign(e.style, v);
    else if (k.startsWith('on')) e.addEventListener(k.slice(2), v);
    else if (v === true) e.setAttribute(k, '');
    else e.setAttribute(k, v);
  }
  for (const c of children.flat(Infinity)) {
    if (c === null || c === undefined || c === false) continue;
    e.append(c instanceof Node ? c : String(c));
  }
  return e;
}

const storage = {
  get(kind, k) { try { return window[kind].getItem(k); } catch (_) { return null; } },
  set(kind, k, v) { try { window[kind].setItem(k, v); } catch (_) { /* private mode */ } },
  del(kind, k) { try { window[kind].removeItem(k); } catch (_) { /* private mode */ } },
};
const token = () => storage.get('sessionStorage', TOKEN_KEY);
const enc = encodeURIComponent;

const gib = (mib) => (mib === undefined || mib === null ? '–'
  : (mib / 1024).toFixed(mib % 1024 ? 1 : 0).replace(/\.0$/, '') + ' GiB');
const plural = (n, one, many) => n + ' ' + (n === 1 ? one : (many || one + 's'));
const rtf = new Intl.RelativeTimeFormat(undefined, { numeric: 'auto' });
function rel(t) {
  if (!t) return '';
  const s = (new Date(t).getTime() - Date.now()) / 1000;
  const a = Math.abs(s);
  if (a < 45) return s < 0 ? 'just now' : 'in a moment';
  if (a < 3600) return rtf.format(Math.round(s / 60), 'minute');
  if (a < 48 * 3600) return rtf.format(Math.round(s / 3600), 'hour');
  return rtf.format(Math.round(s / 86400), 'day');
}
const when = (t) => (t ? new Date(t).toLocaleString(undefined, { dateStyle: 'medium', timeStyle: 'short' }) : '');
const dur = (sec) => {
  const hrs = sec / 3600;
  return hrs < 1 ? Math.round(sec / 60) + ' min' : hrs < 48 ? Math.round(hrs) + ' h' : Math.round(hrs / 24) + ' days';
};

// ---- API ------------------------------------------------------------------------------------------------------------

class ApiError extends Error {
  constructor(status, message, body) { super(message); this.status = status; this.body = body; }
}

async function api(method, path, { body, text = false } = {}) {
  const headers = {};
  const t = token();
  if (t) headers.Authorization = 'Bearer ' + t;
  if (body !== undefined) headers['Content-Type'] = 'application/json';
  let r;
  try {
    r = await fetch(path, { method, headers, cache: 'no-store', body: body === undefined ? undefined : JSON.stringify(body) });
  } catch (_) {
    throw new ApiError(0, 'Central is not reachable. Check your connection; the console retries every 10 seconds.');
  }
  if (r.status === 204) return {};
  const out = text && r.ok ? await r.text() : await r.json().catch(() => ({}));
  if (!r.ok) throw new ApiError(r.status, (out && out.error) || r.status + ' ' + r.statusText, out);
  return out;
}
const soft = (p, fallback) => p.catch((e) => { if (e.status === 401 || e.status === 503) throw e; return fallback; });

// ---- state ----------------------------------------------------------------------------------------------------------

const S = {
  role: null, // 'operator' | 'tenant'
  me: null,
  data: null,
  loadedAt: 0,
  error: null,
  standby: null,
  usageDays: 30,
  usage: null,
  policy: null,
  vmFilter: 'all',
  query: '',
  exec: {}, // vm key -> { draft, log: [] }
  lastViewKey: '',
  lastInspKey: '',
};

const isOp = () => S.role === 'operator';
const vmKey = (v) => v.node + '/' + v.id;
const guestOf = (req) => ((req && req.apple && req.apple.guest_os) === 'macos' ? 'macos' : 'linux');
const guestName = (g) => (g === 'macos' ? 'macOS' : 'Linux');

function vmStatus(v) {
  const s = typeof v.status === 'string' ? v.status : (v.state || (v.status && Object.keys(v.status)[0]) || 'unknown');
  const cls = { running: 'ok', creating: 'warn', starting: 'warn', paused: 'warn', stopped: 'idle', failed: 'bad' }[s] || 'idle';
  return { text: s[0].toUpperCase() + s.slice(1), cls };
}

function imageFor(v) {
  const path = v.request && v.request.image;
  return (S.data.images || []).find((i) => i.image === path);
}

/** Every Mac this caller can see, with lease, drain and VM information joined in. */
function macs() {
  const d = S.data;
  const leaseOf = Object.fromEntries((d.leases || []).filter((l) => l.state === 'active').map((l) => [l.node, l]));
  const vmsOn = {};
  for (const v of d.vms || []) (vmsOn[v.node] = vmsOn[v.node] || []).push(v);
  const names = isOp()
    ? (d.nodes || []).map((n) => n.name)
    : Object.keys(leaseOf);
  return names.sort().map((name) => {
    const node = (isOp() ? (d.nodes || []).find((n) => n.name === name) : d.leasedNodes[name]) || { name };
    const drain = (d.drains || {})[name] || {};
    const vms = vmsOn[name] || [];
    const macosVms = vms.filter((v) => guestOf(v.request) === 'macos');
    const macosGuests = Math.max(node.apple ? node.apple.macos_guests || 0 : 0, macosVms.length);
    return { name, node, lease: leaseOf[name] || null, drain, vms, macosVms, macosGuests, isMac: !!node.apple };
  });
}

function macState(m) {
  if (m.node.healthy === false) return { text: 'Not responding', cls: 'bad' };
  if (m.drain.scrub_pending) return { text: 'Needs scrub', cls: 'warn' };
  if (m.drain.draining) return { text: 'Draining', cls: 'warn' };
  if (m.node.cordoned) return { text: 'Cordoned', cls: 'warn' };
  if (m.lease) return { text: isOp() ? 'Leased to ' + m.lease.tenant : 'Leased', cls: 'ok' };
  if (!m.node.healthy && m.node.healthy !== false) return { text: 'Unknown', cls: 'idle' };
  return { text: m.vms.length ? 'Shared' : 'Available', cls: m.vms.length ? 'ok' : 'idle' };
}

function leaseProgress(l) {
  const start = new Date(l.started_at).getTime();
  const end = new Date(l.min_end).getTime();
  const p = end > start ? Math.min(1, Math.max(0, (Date.now() - start) / (end - start))) : 1;
  return { p, releasable: Date.now() >= end };
}

// ---- loading --------------------------------------------------------------------------------------------------------

async function detectRole() {
  try {
    S.me = await api('GET', '/fleet/tenants/me');
    S.role = 'tenant';
  } catch (e) {
    if (e.status === 400) { S.role = 'operator'; S.me = null; } else throw e;
  }
}

async function load() {
  if (!S.role) await detectRole();
  else if (S.role === 'tenant') S.me = await api('GET', '/fleet/tenants/me');
  const [leases, vms, overlays, images] = await Promise.all([
    api('GET', '/fleet/leases'),
    api('GET', '/fleet/vms'),
    soft(api('GET', '/fleet/overlays'), { items: [] }),
    soft(api('GET', '/fleet/images'), { items: [] }),
  ]);
  const d = {
    leases: (leases.items || []).sort((a, b) => (a.state === b.state ? b.started_at.localeCompare(a.started_at)
      : a.state === 'active' ? -1 : 1)),
    vms: (vms.items || []).sort((a, b) => String(a.name).localeCompare(String(b.name))),
    unreachable: vms.unreachable_nodes || [],
    overlays: overlays.items || [],
    images: (images.items || []).sort((a, b) => (a.guest_os === b.guest_os ? a.name.localeCompare(b.name) : a.guest_os === 'macos' ? -1 : 1)),
    leasedNodes: {},
    drains: {},
  };
  if (isOp()) {
    const [nodes, tenants, capacity] = await Promise.all([
      api('GET', '/fleet/nodes'),
      soft(api('GET', '/fleet/tenants'), { items: [] }),
      soft(api('GET', '/fleet/capacity'), {}),
    ]);
    d.nodes = nodes.items || [];
    d.tenants = (tenants.items || []).sort((a, b) => a.id.localeCompare(b.id));
    d.capacity = capacity;
    const drains = await Promise.all(d.nodes.map((n) =>
      soft(api('GET', '/fleet/nodes/' + enc(n.name) + '/drain'), {})));
    d.nodes.forEach((n, i) => { d.drains[n.name] = drains[i]; });
  } else {
    const active = d.leases.filter((l) => l.state === 'active');
    const views = await Promise.all(active.map((l) => soft(api('GET', '/fleet/nodes/' + enc(l.node)), { name: l.node })));
    active.forEach((l, i) => { d.leasedNodes[l.node] = views[i]; });
  }
  S.data = d;
}

async function haStatus() {
  try {
    const ha = await fetch('/fleet/ha', { cache: 'no-store' }).then((r) => r.json());
    S.standby = ha.enabled && !ha.is_leader ? (ha.leader || 'unknown') : null;
  } catch (_) { S.standby = null; }
}

let timer = null;
let loading = false;
async function refresh({ force = false } = {}) {
  clearTimeout(timer);
  if (loading) return;
  loading = true;
  try {
    await haStatus();
    await load();
    S.error = null;
    S.loadedAt = Date.now();
    showApp();
  } catch (e) {
    if (e.status === 401) { showSignin(token() ? 'That token was not accepted. Check it and try again.' : ''); loading = false; return; }
    if (e.status === 403 && !S.data) { showSignin('This tenant is disabled. Ask your operator to enable it.'); loading = false; return; }
    S.error = e;
    if (S.data) showApp(); else showSignin(e.message);
  } finally {
    loading = false;
  }
  render(force);
  if (document.visibilityState === 'visible') timer = setTimeout(refresh, 10000);
}
document.addEventListener('visibilitychange', () => { if (document.visibilityState === 'visible') refresh(); });

function showSignin(msg) {
  $('app').hidden = true;
  $('signin').hidden = false;
  $('signin-error').textContent = msg || '';
  $('signin-token').focus();
}
function showApp() {
  $('signin').hidden = true;
  $('app').hidden = false;
}

// ---- routing --------------------------------------------------------------------------------------------------------

function route() {
  const parts = location.hash.replace(/^#\/?/, '').split('/').map(decodeURIComponent);
  const page = parts[0] || 'overview';
  return { page, id: parts.slice(1).filter(Boolean).join('/') || null };
}
const go = (page, id) => { location.hash = '#/' + page + (id ? '/' + id.split('/').map(enc).join('/') : ''); };
window.addEventListener('hashchange', () => render(true));

const PAGES = () => [
  { id: 'overview', label: 'Overview' },
  { id: 'macs', label: isOp() ? 'Macs' : 'My Macs', count: () => macs().length },
  { id: 'vms', label: 'Virtual machines', count: () => S.data.vms.length },
  { id: 'networks', label: 'Networks', count: () => S.data.overlays.length },
  { id: 'images', label: 'Images', count: () => S.data.images.length },
  { sep: true },
  ...(isOp() ? [{ id: 'tenants', label: 'Tenants', count: () => (S.data.tenants || []).length }] : []),
  { id: 'usage', label: 'Usage' },
  ...(isOp() ? [{ id: 'policy', label: 'Policy' }] : []),
];

// ---- render ---------------------------------------------------------------------------------------------------------

function render(force) {
  if (!S.data) return;
  const r = route();
  const pages = PAGES();
  const page = pages.find((p) => p.id === r.page) || pages[0];

  $('nav').replaceChildren(...pages.map((p) => (p.sep ? h('li', { class: 'sep-li' }, h('div', { class: 'sep' }))
    : h('li', {}, h('a', { href: '#/' + p.id, 'aria-current': p.id === page.id ? 'page' : null },
      h('span', {}, p.label), p.count ? h('span', { class: 'count' }, p.count()) : null)))));
  $('who').textContent = isOp() ? 'Operator' : (S.me.display_name || S.me.id);
  $('role').textContent = isOp() ? 'Fleet token' : 'Tenant ' + S.me.id;
  $('bar-title').textContent = page.label;
  document.title = page.label + ' · FluxVM Cloud';
  $('signout').hidden = !token();
  renderBanners();
  tick();

  const viewKey = JSON.stringify([page.id, r.id, S.data, S.me, S.usage, S.usageDays, S.policy, S.vmFilter, S.query]);
  if (force || viewKey !== S.lastViewKey) {
    S.lastViewKey = viewKey;
    const view = $('view');
    view.replaceChildren(...[].concat(VIEWS[page.id]()));
  }
  renderInspector(page.id, r.id, force);
}

function renderBanners() {
  const out = [];
  if (S.standby) {
    const link = /^https?:\/\//.test(S.standby) ? h('a', { href: S.standby + '/console' }, S.standby) : S.standby;
    out.push(h('div', { class: 'banner warn', role: 'status' },
      h('span', {}, 'This central is on standby, so changes are refused here. The leader is ', link, '.')));
  }
  if (S.error) {
    out.push(h('div', { class: 'banner bad', role: 'alert' },
      h('span', { class: 'grow' }, S.error.message + (S.loadedAt ? ' Showing what was loaded ' + rel(S.loadedAt) + '.' : '')),
      h('button', { onclick: () => refresh({ force: true }) }, 'Try again')));
  }
  $('banners').replaceChildren(...out);
}

function tick() {
  $('updated').textContent = S.loadedAt ? 'Updated ' + rel(S.loadedAt) : '';
}
setInterval(tick, 15000);

// Page parts

function pageHead(title, sub, ...actions) {
  return h('div', { class: 'page-head' },
    h('div', {}, h('h1', {}, title), sub ? h('p', {}, sub) : null),
    actions.length ? h('div', { class: 'actions' }, actions) : null);
}

function empty(title, text, ...actions) {
  return h('div', { class: 'panel empty' }, h('h3', {}, title), h('p', {}, text),
    actions.length ? h('div', { class: 'row', style: { justifyContent: 'center' } }, actions) : null);
}

const status = (s) => h('span', { class: 'status ' + s.cls }, s.text);

function meter(label, used, max, fmt = String) {
  const has = max !== undefined && max !== null && max > 0;
  const p = has ? Math.min(1, used / max) : 0;
  return h('div', { class: 'panel meter' },
    h('div', { class: 'muted small' }, label),
    h('div', { class: 'value' }, fmt(used), has ? h('small', {}, ' of ' + fmt(max)) : h('small', {}, ' (no limit)')),
    has ? h('div', { class: 'track' + (p >= 1 ? ' bad' : p >= 0.8 ? ' warn' : '') },
      h('i', { style: { width: (p * 100).toFixed(1) + '%' } })) : null);
}

function chassis(m, big) {
  const bays = [0, 1].map((i) => {
    const v = m.macosVms[i];
    const used = i < m.macosGuests;
    return h('span', { class: 'bay' + (used ? ' used' : ''), title: used ? 'macOS guest' + (v ? ': ' + v.name : '') : 'Free macOS slot' },
      used ? (v ? v.name : 'macOS') : 'macOS slot');
  });
  return h('div', { class: 'chassis' + (big ? ' big' : ''), role: 'img',
    'aria-label': m.macosGuests + ' of 2 macOS slots in use' },
  bays, h('span', { class: 'led' + (m.node.healthy !== false ? ' on' : '') }));
}

function ring(l, big) {
  const { p, releasable } = leaseProgress(l);
  return h('span', { class: 'ring' + (releasable ? ' done' : '') + (big ? ' big' : ''), style: { '--p': p.toFixed(3) },
    role: 'img', 'aria-label': releasable ? 'Minimum lease period over' : Math.round(p * 100) + '% of the minimum lease period' });
}

function leaseLine(l) {
  if (!l) return '';
  const { releasable } = leaseProgress(l);
  return releasable ? 'Can be released now' : 'Can be released ' + rel(l.min_end);
}

function macTile(m) {
  const st = macState(m);
  const n = m.node;
  const sel = route().page === 'macs' && route().id === m.name;
  return h('button', { class: 'mac' + (sel ? ' selected' : ''), type: 'button', onclick: () => go('macs', m.name) },
    h('div', { class: 'mac-top' }, h('b', { class: 'grow' }, m.name), status(st)),
    m.isMac ? chassis(m) : h('div', { class: 'chassis' }, h('span', { class: 'muted small' }, 'Linux host'),
      h('span', { class: 'led' + (n.healthy !== false ? ' on' : '') })),
    h('div', { class: 'mac-meta' },
      m.lease ? ring(m.lease) : null,
      h('div', { class: 'grow' },
        h('div', { class: 'small' }, n.vcpus_total ? n.vcpus_total + ' vCPUs, ' + gib(n.memory_mib_total) : 'Size unknown'),
        h('div', { class: 'small muted' }, m.lease ? leaseLine(m.lease)
          : plural(m.vms.length, 'VM') + (n.free_vcpus !== undefined ? ', ' + n.free_vcpus + ' vCPUs free' : '')))));
}

// ---- views ----------------------------------------------------------------------------------------------------------

const VIEWS = {
  overview() { return isOp() ? opOverview() : tenantOverview(); },

  macs() {
    const list = macs();
    if (!isOp()) {
      return [pageHead('My Macs', 'A leased Mac is yours alone: only your VMs run on it, up to two of them macOS. '
          + 'Leases last at least 24 hours, as Apple\'s macOS licence requires.', h('button', { class: 'primary', onclick: leaseSheet }, 'Lease a Mac')),
      list.length ? h('div', { class: 'macs' }, list.map(macTile))
        : empty('You have no Macs yet', 'Lease a Mac to run macOS VMs. It is yours alone until you release it, '
          + 'and you can release it once the 24-hour minimum is over.', h('button', { class: 'primary', onclick: leaseSheet }, 'Lease a Mac'))];
    }
    return [pageHead('Macs', 'Every host that reports to this central. Select one to cordon, drain, scrub or lease it.',
      h('button', { onclick: leaseSheet }, 'Lease a Mac to a tenant')),
    list.length ? h('div', { class: 'macs' }, list.map(macTile))
      : empty('No Macs have registered', 'Run fluxvm-agent node on each Mac, pointing --central at this server. '
        + 'To try the console without Macs, run scripts/fleet-sim.sh.')];
  },

  vms() {
    const q = S.query.trim().toLowerCase();
    const all = S.data.vms;
    const filters = [['all', 'All'], ['running', 'Running'], ['other', 'Not running']];
    const list = all.filter((v) => {
      const s = vmStatus(v).text.toLowerCase();
      if (S.vmFilter === 'running' && s !== 'running') return false;
      if (S.vmFilter === 'other' && s === 'running') return false;
      return !q || [v.name, v.node, v.id, (imageFor(v) || {}).name].some((x) => String(x || '').toLowerCase().includes(q));
    });
    const sel = route().id;
    const out = [pageHead('Virtual machines', isOp() ? 'Every VM created through this central, on every Mac.'
      : 'Your VMs, on the Macs you lease and on shared hosts.', h('button', { class: 'primary', onclick: () => launchSheet() }, 'Launch VM'))];
    if (S.data.unreachable.length) {
      out.push(h('div', { class: 'note warn', style: { marginBottom: '16px' } },
        'Not listed because they did not answer: ' + S.data.unreachable.map((u) => u.node).join(', ') + '.'));
    }
    if (!all.length) {
      out.push(empty('No virtual machines yet', isOp() ? 'VMs appear here as tenants launch them.'
        : (S.data.leases.some((l) => l.state === 'active') ? 'Launch a VM from an image in the catalog.'
          : 'macOS VMs run on Macs you lease. Lease one, then launch a VM on it.'),
      !isOp() && !S.data.leases.some((l) => l.state === 'active') ? h('button', { onclick: leaseSheet }, 'Lease a Mac') : null,
      h('button', { class: 'primary', onclick: () => launchSheet() }, 'Launch VM')));
      return out;
    }
    out.push(h('div', { class: 'row', style: { marginBottom: '12px' } },
      h('div', { class: 'seg', role: 'group', 'aria-label': 'Filter' }, filters.map(([id, label]) =>
        h('button', { 'aria-pressed': S.vmFilter === id ? 'true' : 'false', onclick: () => { S.vmFilter = id; render(); } }, label))),
      q ? h('span', { class: 'muted small' }, plural(list.length, 'match', 'matches') + ' for “' + S.query + '”') : null));
    out.push(h('div', { class: 'panel table-wrap' }, h('table', {},
      h('thead', {}, h('tr', {}, h('th', {}, 'Name'), h('th', {}, 'Status'), h('th', { class: 'hide-sm' }, 'Image'),
        h('th', {}, 'Mac'), h('th', { class: 'r hide-sm' }, 'Size'), isOp() ? h('th', { class: 'hide-sm' }, 'Tenant') : null,
        h('th', { class: 'hide-sm' }, 'Created'))),
      h('tbody', {}, list.length ? list.map((v) => {
        const req = v.request || {};
        const img = imageFor(v);
        return h('tr', { class: 'link' + (sel === vmKey(v) ? ' selected' : ''), tabindex: '0',
          onclick: () => go('vms', vmKey(v)), onkeydown: (e) => { if (e.key === 'Enter') go('vms', vmKey(v)); } },
        h('td', {}, h('b', {}, v.name), h('span', { class: 'sub' }, guestName(guestOf(req)))),
        h('td', {}, status(vmStatus(v))),
        h('td', { class: 'hide-sm' }, img ? img.display_name || img.name : h('span', { class: 'muted' }, 'Custom disk')),
        h('td', {}, v.node),
        h('td', { class: 'r num hide-sm' }, (req.vcpus || '–') + ' vCPUs, ' + gib(req.memory_mib)),
        isOp() ? h('td', { class: 'hide-sm' }, req.tenant || h('span', { class: 'muted' }, 'none')) : null,
        h('td', { class: 'hide-sm muted' }, rel(v.created_at)));
      }) : h('tr', {}, h('td', { colspan: '7', class: 'muted' }, 'Nothing matches.'))))));
    return out;
  },

  networks() {
    const list = S.data.overlays;
    const out = [pageHead('Private networks', 'A private network connects your VMs across Macs as if they shared one '
      + 'switch. Traffic between Macs is encrypted.', h('button', { class: 'primary', onclick: networkSheet }, 'Create network'))];
    if (!list.length) {
      out.push(empty('No private networks', 'Create one, then pick it when you launch a VM. VMs on it reach each '
        + 'other by IP on any of its Macs.', h('button', { class: 'primary', onclick: networkSheet }, 'Create network')));
      return out;
    }
    const sel = route().id;
    out.push(h('div', { class: 'panel table-wrap' }, h('table', {},
      h('thead', {}, h('tr', {}, h('th', {}, 'Network'), h('th', {}, 'Subnet'), h('th', {}, 'Macs'),
        isOp() ? h('th', { class: 'hide-sm' }, 'Tenant') : null, h('th', { class: 'hide-sm' }, 'Key rotated'))),
      h('tbody', {}, list.map((n) => h('tr', { class: 'link' + (sel === n.name ? ' selected' : ''), tabindex: '0',
        onclick: () => go('networks', n.name), onkeydown: (e) => { if (e.key === 'Enter') go('networks', n.name); } },
      h('td', {}, h('b', {}, n.name)),
      h('td', {}, h('code', {}, n.subnet)),
      h('td', {}, Object.keys(n.members || {}).join(', ') || h('span', { class: 'muted' }, 'none yet')),
      isOp() ? h('td', { class: 'hide-sm' }, n.tenant || h('span', { class: 'muted' }, 'operator')) : null,
      h('td', { class: 'hide-sm muted' }, rel(n.key_rotated_at))))))));
    return out;
  },

  images() {
    const list = S.data.images;
    const out = [pageHead('Images', isOp() ? 'The images tenants can launch. Pull each one onto your Macs first '
      + '(fluxctl vm-image pull), then add it here with its path on the Macs.' : 'Images your operator has prepared on the Macs.',
    isOp() ? h('button', { class: 'primary', onclick: () => imageSheet() }, 'Add image') : null)];
    if (!list.length) {
      out.push(isOp()
        ? empty('The catalog is empty', 'Add the images you have pulled onto your Macs, so tenants can launch them '
          + 'by name instead of typing disk paths.', h('button', { class: 'primary', onclick: () => imageSheet() }, 'Add image'))
        : empty('No images yet', 'Your operator has not added any images. You can still launch from a disk path: '
          + 'choose Custom disk in Launch VM.', h('button', { onclick: () => launchSheet() }, 'Launch VM')));
      return out;
    }
    out.push(h('div', { class: 'macs' }, list.map((i) => h('div', { class: 'panel pad image-card' },
      h('div', { class: 'row' }, h('h3', { class: 'grow' }, i.display_name || i.name), h('span', { class: 'tag' }, guestName(i.guest_os))),
      h('p', { class: 'small muted', style: { marginTop: '6px', minHeight: '20px' } }, i.description || i.source || i.name),
      h('p', { class: 'small', style: { marginTop: '10px' } }, i.vcpus + ' vCPUs, ' + gib(i.memory_mib)
        + (i.disk_size_gib ? ', ' + i.disk_size_gib + ' GiB disk' : '')),
      h('div', { class: 'row', style: { marginTop: 'auto', paddingTop: '14px' } },
        h('button', { class: 'primary', onclick: () => launchSheet({ image: i.name }) }, 'Launch'),
        isOp() ? h('button', { onclick: () => imageSheet(i) }, 'Edit') : null,
        isOp() ? h('button', { class: 'plain danger', onclick: () => removeImage(i) }, 'Remove') : null)))));
    return out;
  },

  usage() {
    if (!S.usage) { loadUsage(); return [pageHead('Usage'), h('p', { class: 'muted' }, 'Loading…')]; }
    const rows = (S.usage.tenants || []).filter((t) => isOp() || t.tenant === S.me.id);
    const hrs = (s) => (s / 3600).toFixed(1);
    const seg = h('div', { class: 'seg', role: 'group', 'aria-label': 'Period' }, [7, 30, 90].map((d) =>
      h('button', { 'aria-pressed': S.usageDays === d ? 'true' : 'false', onclick: () => { S.usageDays = d; S.usage = null; render(); } },
        d + ' days')));
    const out = [pageHead('Usage', 'Leases are billed for at least 24 hours each, even when released sooner.',
      seg, h('button', { onclick: downloadCsv }, 'Download CSV'))];
    if (!rows.length) { out.push(empty('No usage in this period', 'Usage appears once VMs run or Macs are leased.')); return out; }
    if (!isOp()) {
      const t = rows[0];
      out.push(h('div', { class: 'meters' },
        h('div', { class: 'panel meter' }, h('div', { class: 'muted small' }, 'Billable Mac hours'), h('div', { class: 'value' }, hrs(t.billable_lease_seconds))),
        h('div', { class: 'panel meter' }, h('div', { class: 'muted small' }, 'Leased Mac hours'), h('div', { class: 'value' }, hrs(t.lease_seconds))),
        h('div', { class: 'panel meter' }, h('div', { class: 'muted small' }, 'VM hours'), h('div', { class: 'value' }, hrs(t.vm_seconds)),
          h('div', { class: 'small muted' }, hrs(t.macos_vm_seconds) + ' of them macOS')),
        h('div', { class: 'panel meter' }, h('div', { class: 'muted small' }, 'vCPU hours'), h('div', { class: 'value' }, hrs(t.vcpu_seconds)))));
      return out;
    }
    const max = Math.max(1, ...rows.map((t) => t.billable_lease_seconds));
    out.push(h('div', { class: 'panel pad' }, h('h3', { style: { marginBottom: '10px' } }, 'Billable Mac hours by tenant'),
      h('div', { class: 'bars' }, rows.map((t) => h('div', {},
        h('span', {}, t.tenant || 'No tenant'),
        h('span', { class: 'bar-track' }, h('i', { style: { width: (100 * t.billable_lease_seconds / max).toFixed(1) + '%' } })),
        h('span', { class: 'num r', style: { textAlign: 'right' } }, hrs(t.billable_lease_seconds) + ' h'))))));
    out.push(h('div', { class: 'panel table-wrap section' }, h('table', {},
      h('thead', {}, h('tr', {}, ['Tenant', 'VM hours', 'vCPU hours', 'Memory GiB-hours', 'macOS VM hours', 'Lease hours', 'Billable']
        .map((x, i) => h('th', { class: i ? 'r' : '' }, x)))),
      h('tbody', {}, rows.map((t) => h('tr', {},
        h('td', {}, t.tenant || h('span', { class: 'muted' }, 'No tenant')),
        ...[t.vm_seconds, t.vcpu_seconds].map((s) => h('td', { class: 'r num' }, hrs(s))),
        h('td', { class: 'r num' }, (t.memory_mib_seconds / 1024 / 3600).toFixed(1)),
        ...[t.macos_vm_seconds, t.lease_seconds, t.billable_lease_seconds].map((s) => h('td', { class: 'r num' }, hrs(s)))))))));
    return out;
  },

  tenants() {
    const list = S.data.tenants || [];
    const out = [pageHead('Tenants', 'Each tenant signs in with its own token and sees only its own Macs, VMs and networks.',
      h('button', { class: 'primary', onclick: tenantSheet }, 'Create tenant'))];
    if (!list.length) {
      out.push(empty('No tenants yet', 'Until you create one, the fleet has a single user: whoever holds the fleet token.',
        h('button', { class: 'primary', onclick: tenantSheet }, 'Create tenant')));
      return out;
    }
    const sel = route().id;
    const use = (u, max) => (max === undefined || max === null ? String(u) : u + ' of ' + max);
    out.push(h('div', { class: 'panel table-wrap' }, h('table', {},
      h('thead', {}, h('tr', {}, h('th', {}, 'Tenant'), h('th', {}, 'Macs'), h('th', {}, 'VMs'),
        h('th', { class: 'hide-sm' }, 'vCPUs'), h('th', { class: 'hide-sm' }, 'Memory'), h('th', {}, 'Status'))),
      h('tbody', {}, list.map((t) => {
        const q = t.quota || {};
        const u = t.in_use || {};
        return h('tr', { class: 'link' + (sel === t.id ? ' selected' : ''), tabindex: '0',
          onclick: () => go('tenants', t.id), onkeydown: (e) => { if (e.key === 'Enter') go('tenants', t.id); } },
        h('td', {}, h('b', {}, t.display_name || t.id), t.display_name ? h('span', { class: 'sub' }, t.id) : null),
        h('td', { class: 'num' }, use(u.leased_hosts || 0, q.max_leased_hosts)),
        h('td', { class: 'num' }, use(u.vms || 0, q.max_vms)),
        h('td', { class: 'num hide-sm' }, use(u.vcpus || 0, q.max_vcpus)),
        h('td', { class: 'num hide-sm' }, q.max_memory_mib ? gib(u.memory_mib || 0) + ' of ' + gib(q.max_memory_mib) : gib(u.memory_mib || 0)),
        h('td', {}, status(t.disabled ? { text: 'Disabled', cls: 'bad' } : { text: 'Active', cls: 'ok' })));
      })))));
    return out;
  },

  policy() {
    if (!S.policy) { loadPolicy(); return [pageHead('Policy'), h('p', { class: 'muted' }, 'Loading…')]; }
    return policyView();
  },
};

function tenantOverview() {
  const me = S.me;
  const q = me.quota || {};
  const u = me.in_use || {};
  const list = macs();
  const hasLease = list.length > 0;
  const hasVm = S.data.vms.length > 0;
  const hasNet = S.data.overlays.length > 0;
  const out = [pageHead(me.display_name || me.id, 'Your Macs, VMs and quota at a glance.')];
  if (!(hasLease && hasVm && hasNet)) {
    const step = (done, n, title, text, btn) => h('li', { class: done ? 'done' : '' },
      h('span', { class: 'n', 'aria-hidden': 'true' }, done ? '✓' : n),
      h('div', { class: 'grow' }, h('h3', {}, title), h('p', { class: 'small muted' }, text)),
      done ? null : btn);
    out.push(h('div', { class: 'panel pad' }, h('h2', { style: { marginBottom: '6px' } }, 'Get started'),
      h('ol', { class: 'steps' },
        step(hasLease, 1, 'Lease a Mac', 'A whole Mac for you alone, for at least 24 hours.',
          h('button', { class: 'primary', onclick: leaseSheet }, 'Lease a Mac')),
        step(hasVm, 2, 'Launch a VM', 'Pick an image; it starts in seconds on your Mac.',
          h('button', { class: hasLease ? 'primary' : '', onclick: () => launchSheet() }, 'Launch VM')),
        step(hasNet, 3, 'Connect your VMs', 'Optional: a private network across your Macs.',
          h('button', { onclick: networkSheet }, 'Create network')))));
  }
  out.push(h('div', { class: 'section meters' },
    meter('Macs leased', u.leased_hosts || 0, q.max_leased_hosts),
    meter('VMs', u.vms || 0, q.max_vms),
    meter('vCPUs', u.vcpus || 0, q.max_vcpus),
    meter('Memory', u.memory_mib || 0, q.max_memory_mib, gib)));
  if (list.length) {
    out.push(h('div', { class: 'section' }, h('div', { class: 'section-head' }, h('h2', { class: 'grow' }, 'Your Macs'),
      h('button', { class: 'plain', onclick: leaseSheet }, 'Lease another')), h('div', { class: 'macs' }, list.map(macTile))));
  }
  if (S.data.vms.length) {
    out.push(h('div', { class: 'section' }, h('div', { class: 'section-head' }, h('h2', { class: 'grow' }, 'Recent VMs'),
      h('a', { href: '#/vms' }, 'All VMs')),
    h('div', { class: 'panel list' }, [...S.data.vms].sort((a, b) => String(b.created_at).localeCompare(String(a.created_at)))
      .slice(0, 5).map((v) => h('a', { href: '#/vms/' + enc(v.node) + '/' + enc(v.id), style: { color: 'inherit' } },
        h('b', { class: 'grow' }, v.name), h('span', { class: 'muted small hide-sm' }, v.node), status(vmStatus(v)))))));
  }
  return out;
}

function opOverview() {
  const d = S.data;
  const c = d.capacity || {};
  const list = macs();
  const attention = [];
  for (const m of list) {
    if (m.node.healthy === false) attention.push([m, 'bad', m.name + ' has not reported for over 30 seconds.', 'Check the Mac and its fluxvm-agent node.']);
    if (m.drain.scrub_pending) attention.push([m, 'warn', m.name + ' needs a scrub before anyone else can lease it.', 'Its last lease ended.']);
    if (m.drain.draining && m.drain.done) attention.push([m, 'warn', m.name + ' has finished draining.', 'Uncordon it when maintenance is done.']);
    if (m.isMac && m.macosGuests >= 2 && !m.lease) attention.push([m, 'warn', m.name + ' runs two macOS guests without a lease.', 'Apple\'s licence expects a lessee.']);
  }
  for (const u of d.unreachable) attention.push([null, 'bad', u.node + ' did not answer the VM list.', u.reason || '']);
  const out = [pageHead('Overview', plural(list.length, 'Mac') + ', ' + plural(d.vms.length, 'VM') + ', '
    + plural((d.tenants || []).length, 'tenant') + '.')];
  out.push(h('div', { class: 'panel' }, attention.length
    ? h('div', { class: 'list' }, attention.map(([m, cls, title, text]) => h('div', {},
      h('span', { class: 'status ' + cls, 'aria-hidden': 'true' }),
      h('div', { class: 'grow' }, h('div', {}, title), text ? h('div', { class: 'small muted' }, text) : null),
      m ? h('button', { onclick: () => go('macs', m.name) }, 'Open') : null)))
    : h('div', { class: 'list' }, h('div', {}, h('span', { class: 'status ok', 'aria-hidden': 'true' }),
      h('span', {}, 'Every Mac is reporting. Nothing needs you right now.')))));
  out.push(h('div', { class: 'section meters' },
    meter('Healthy Macs', c.nodes_healthy ?? 0, c.nodes_total),
    meter('vCPUs in use', (c.vcpus_total || 0) - (c.vcpus_free || 0), c.vcpus_total),
    meter('Memory in use', (c.memory_mib_total || 0) - (c.memory_mib_free || 0), c.memory_mib_total, gib),
    meter('Macs leased', d.leases.filter((l) => l.state === 'active').length, list.filter((m) => m.isMac).length)));
  out.push(h('div', { class: 'section' }, h('div', { class: 'section-head' }, h('h2', { class: 'grow' }, 'Macs'),
    h('a', { href: '#/macs' }, 'Manage')),
  list.length ? h('div', { class: 'macs' }, list.map(macTile))
    : empty('No Macs have registered', 'Run fluxvm-agent node on each Mac, or scripts/fleet-sim.sh to try it out.')));
  return out;
}

async function loadUsage() {
  const to = new Date();
  const from = new Date(to.getTime() - S.usageDays * 86400000);
  try {
    S.usage = await api('GET', '/fleet/usage?from=' + enc(from.toISOString()) + '&to=' + enc(to.toISOString()));
    render();
  } catch (e) { toast(e.message, true); }
}

async function downloadCsv() {
  try {
    const to = new Date();
    const from = new Date(to.getTime() - S.usageDays * 86400000);
    const csv = await api('GET', '/fleet/usage?format=csv&from=' + enc(from.toISOString()) + '&to=' + enc(to.toISOString()), { text: true });
    const url = URL.createObjectURL(new Blob([csv], { type: 'text/csv' }));
    const a = h('a', { href: url, download: 'fluxvm-usage-' + S.usageDays + 'd.csv' });
    document.body.append(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(url), 60000);
    toast('Downloaded usage for the last ' + S.usageDays + ' days');
  } catch (e) { toast(e.message, true); }
}

// ---- policy ---------------------------------------------------------------------------------------------------------

async function loadPolicy() {
  try { S.policy = await api('GET', '/fleet/policy'); render(); } catch (e) { toast(e.message, true); }
}

function policyView() {
  const p = S.policy;
  const form = h('form', { class: 'panel pad stack', onsubmit: async (e) => {
    e.preventDefault();
    const f = new FormData(e.target);
    const next = {
      ...p,
      require_lease_for_macos: f.has('require_lease_for_macos'),
      exclusive_leased_hosts: f.has('exclusive_leased_hosts'),
      isolate_tenant_networks: f.has('isolate_tenant_networks'),
      min_lease_hours: Number(f.get('min_lease_hours')),
      ledger_retention_days: Number(f.get('ledger_retention_days')),
      overlay_key_rotation_days: Number(f.get('overlay_key_rotation_days')),
      tenant_subnet_prefix: String(f.get('tenant_subnet_prefix')).trim(),
    };
    const btn = e.submitter;
    btn.disabled = true;
    try { S.policy = await api('PUT', '/fleet/policy', { body: next }); toast('Policy saved'); render(true); } catch (err) { toast(err.message, true); btn.disabled = false; }
  } },
  h('h2', {}, 'Leases'),
  check('require_lease_for_macos', p.require_lease_for_macos, 'macOS VMs need a leased Mac',
    'A tenant\'s macOS guests run only on Macs it leases. Apple\'s licence allows macOS VMs on leased Macs only for a single lessee.'),
  check('exclusive_leased_hosts', p.exclusive_leased_hosts, 'Leased Macs run only the lessee\'s VMs', 'Nobody else, including you, places VMs on a leased Mac.'),
  h('label', { class: 'field', style: { maxWidth: '260px' } }, h('span', {}, 'Minimum lease'),
    h('input', { name: 'min_lease_hours', type: 'number', min: '24', step: '1', value: String(p.min_lease_hours) }),
    h('div', { class: 'hint' }, 'Hours. At least 24, as Apple\'s licence requires.')),
  h('h2', { style: { paddingTop: '12px' } }, 'Networks'),
  check('isolate_tenant_networks', p.isolate_tenant_networks, 'Give each tenant its own network', 'Each tenant\'s VMs share a /24 that no other tenant can reach.'),
  h('div', { class: 'grid2' },
    h('label', { class: 'field' }, h('span', {}, 'Tenant subnet prefix'),
      h('input', { name: 'tenant_subnet_prefix', value: p.tenant_subnet_prefix, pattern: '\\d{1,3}\\.\\d{1,3}' }),
      h('div', { class: 'hint' }, 'Tenant networks are prefix.N.0/24.')),
    h('label', { class: 'field' }, h('span', {}, 'Rotate network keys every'),
      h('input', { name: 'overlay_key_rotation_days', type: 'number', min: '0', value: String(p.overlay_key_rotation_days) }),
      h('div', { class: 'hint' }, 'Days. 0 turns automatic rotation off.'))),
  h('h2', { style: { paddingTop: '12px' } }, 'Records'),
  h('label', { class: 'field', style: { maxWidth: '260px' } }, h('span', {}, 'Keep VM and lease records for'),
    h('input', { name: 'ledger_retention_days', type: 'number', min: '1', value: String(p.ledger_retention_days) }),
    h('div', { class: 'hint' }, 'Days. Usage reports cannot reach further back.')),
  h('div', { class: 'row', style: { paddingTop: '8px' } }, h('span', { class: 'grow' }),
    h('button', { type: 'button', onclick: reconcileNow }, 'Reconcile now'),
    h('button', { class: 'primary', type: 'submit' }, 'Save changes')));
  return [pageHead('Policy', 'Rules central enforces for every tenant.'), form];
}

function check(name, checked, title, text) {
  return h('label', { class: 'check' }, h('input', { type: 'checkbox', name, checked: !!checked }),
    h('span', {}, h('b', { style: { fontWeight: 500 } }, title), h('span', { class: 'hint', style: { display: 'block' } }, text)));
}

async function reconcileNow() {
  try {
    await api('POST', '/fleet/reconcile');
    toast('Reconciled: expired leases ended and the VM records checked against every Mac');
    refresh({ force: true });
  } catch (e) { toast(e.message, true); }
}

// ---- inspector ------------------------------------------------------------------------------------------------------

function renderInspector(page, id, force) {
  const insp = $('inspector');
  let content = null;
  let key = null;
  if (id) {
    if (page === 'macs') { const m = macs().find((x) => x.name === id); key = m && JSON.stringify(m); content = m ? () => macInspector(m) : null; }
    if (page === 'vms') { const v = S.data.vms.find((x) => vmKey(x) === id); key = v && JSON.stringify([v, S.data.images]); content = v ? () => vmInspector(v) : null; }
    if (page === 'networks') { const n = S.data.overlays.find((x) => x.name === id); key = n && JSON.stringify([n, S.data.leases]); content = n ? () => netInspector(n) : null; }
    if (page === 'tenants') { const t = (S.data.tenants || []).find((x) => x.id === id); key = t && JSON.stringify([t, S.data.leases]); content = t ? () => tenantInspector(t) : null; }
    if (!content) {
      key = 'gone:' + page + id;
      content = () => [h('div', { class: 'head' }, h('h2', { class: 'grow' }, 'Not found'), closeBtn(page)),
        h('p', { class: 'muted' }, 'It may have been deleted, or it belongs to someone else.')];
    }
  }
  $('app').classList.toggle('with-inspector', !!content);
  insp.hidden = !content;
  if (!content) { S.lastInspKey = ''; return; }
  key = page + ':' + key;
  if (!force && key === S.lastInspKey) return;
  // Keep focus and typing in the inspector across refreshes.
  const active = insp.contains(document.activeElement) ? document.activeElement.id : null;
  S.lastInspKey = key;
  insp.replaceChildren(...[].concat(content()));
  if (active && $(active)) $(active).focus();
}

const closeBtn = (page) => h('button', { class: 'icon', 'aria-label': 'Close details', onclick: () => go(page) }, 'Close');

function facts(pairs) {
  return h('dl', { class: 'facts' }, pairs.filter(Boolean).map(([k, v]) => [h('dt', {}, k), h('dd', {}, v)]));
}

function copyField(text, label) {
  return h('div', { class: 'copy-field' }, h('code', {}, text),
    h('button', { class: 'icon', 'aria-label': 'Copy ' + (label || ''), onclick: () => copy(text, label) }, 'Copy'));
}

async function copy(text, label) {
  try { await navigator.clipboard.writeText(text); toast((label || 'Text') + ' copied'); } catch (_) { toast('Copy failed: select the text and copy it yourself', true); }
}

function macInspector(m) {
  const n = m.node;
  const l = m.lease;
  const st = macState(m);
  const name = enc(m.name);
  const a = n.apple || {};
  const out = [h('div', { class: 'head' }, h('div', { class: 'grow' }, h('h2', {}, m.name), status(st)), closeBtn('macs'))];
  out.push(h('section', {}, m.isMac ? chassis(m, true) : null,
    h('p', { class: 'small muted', style: { marginTop: '8px' } }, m.isMac
      ? m.macosGuests + ' of 2 macOS slots in use. Apple\'s licence allows two macOS VMs per Mac.'
      : 'This host has no Apple capabilities: it runs Linux VMs only.')));
  if (n.vcpus_total) {
    const usedC = n.vcpus_total - (n.free_vcpus ?? n.vcpus_total);
    const usedM = n.memory_mib_total - (n.free_memory_mib ?? n.memory_mib_total);
    out.push(h('section', {}, h('h3', {}, 'Capacity'),
      h('div', { class: 'small' }, usedC + ' of ' + n.vcpus_total + ' vCPUs in use'),
      h('div', { class: 'track' }, h('i', { style: { width: (100 * usedC / n.vcpus_total).toFixed(1) + '%' } })),
      h('div', { class: 'small', style: { marginTop: '12px' } }, gib(usedM) + ' of ' + gib(n.memory_mib_total) + ' memory in use'),
      h('div', { class: 'track' }, h('i', { style: { width: (100 * usedM / Math.max(1, n.memory_mib_total)).toFixed(1) + '%' } }))));
  }
  if (l) {
    const { releasable } = leaseProgress(l);
    out.push(h('section', {}, h('h3', {}, 'Lease'),
      h('div', { class: 'row', style: { alignItems: 'center', gap: '14px', marginBottom: '12px' } }, ring(l, true),
        h('div', {}, h('div', {}, leaseLine(l)), h('div', { class: 'small muted' }, 'Billed for at least 24 hours'))),
      facts([isOp() && ['Tenant', h('a', { href: '#/tenants/' + enc(l.tenant) }, l.tenant)], ['Started', when(l.started_at)],
        ['Minimum until', when(l.min_end)], ['Ends', l.ends_at ? when(l.ends_at) : 'When released']]),
      isOp() || releasable ? null : h('p', { class: 'hint' }, 'You can release this Mac ' + rel(l.min_end) + '.')));
  }
  out.push(h('section', {}, h('h3', {}, 'VMs on this Mac'), m.vms.length
    ? h('div', { class: 'stack' }, m.vms.map((v) => h('div', { class: 'row' },
      h('a', { class: 'grow', href: '#/vms/' + enc(v.node) + '/' + enc(v.id) }, v.name), h('span', { class: 'muted small' }, guestName(guestOf(v.request))),
      status(vmStatus(v)))))
    : h('p', { class: 'muted small' }, 'None.')));
  if (m.isMac) {
    out.push(h('section', {}, h('h3', {}, 'Capabilities'), h('div', { class: 'row' },
      [a.nested_virtualization && 'Nested virtualization', a.vmnet && 'vmnet networks', a.secure_boot && 'Secure Boot',
        a.rosetta && 'Rosetta', a.custom_virtio && 'Custom Virtio'].filter(Boolean).map((x) => h('span', { class: 'tag' }, x)))));
  }
  if (isOp()) {
    out.push(h('section', {}, h('h3', {}, 'Details'), facts([
      ['Last report', rel(n.last_seen)], ['Address', h('code', {}, n.fluxvm_url || '')],
      Object.keys(n.labels || {}).length ? ['Labels', h('div', { class: 'row' }, Object.entries(n.labels).map(([k, v]) => h('span', { class: 'tag' }, k + '=' + v)))] : null,
    ])));
  }

  const acts = [];
  if (!isOp()) {
    acts.push(h('button', { class: 'primary', onclick: () => launchSheet({ node: m.name }) }, 'Launch VM on this Mac'));
    if (l) {
      const { releasable } = leaseProgress(l);
      acts.push(h('button', { class: 'danger', disabled: !releasable, title: releasable ? null : 'Available ' + rel(l.min_end),
        onclick: () => releaseLease(l, m) }, 'Release Mac'));
    }
  } else {
    if (n.cordoned) acts.push(h('button', { onclick: () => doAct('POST', '/fleet/nodes/' + name + '/uncordon', m.name + ' uncordoned: it takes new VMs again') }, 'Uncordon'));
    else acts.push(h('button', { onclick: () => doAct('POST', '/fleet/nodes/' + name + '/cordon', m.name + ' cordoned: no new VMs go to it') }, 'Cordon'));
    acts.push(h('button', { onclick: () => drainSheet(m) }, 'Drain…'));
    if (m.drain.scrub_pending) acts.push(h('button', { class: 'danger', onclick: () => scrubMac(m) }, 'Scrub…'));
    if (l) acts.push(h('button', { class: 'danger', onclick: () => releaseLease(l, m) }, leaseProgress(l).releasable ? 'Release lease' : 'Force release…'));
    else if (m.isMac && !m.drain.scrub_pending) acts.push(h('button', { onclick: () => leaseSheet({ node: m.name }) }, 'Lease to a tenant…'));
  }
  out.push(h('section', {}, h('div', { class: 'actions-col' }, acts)));
  return out;
}

function vmInspector(v) {
  const req = v.request || {};
  const img = imageFor(v);
  const k = vmKey(v);
  const ex = S.exec[k] = S.exec[k] || { draft: '', log: [] };
  const nets = ((req.apple || {}).networks || []).map((n) => n.name);
  const running = vmStatus(v).text === 'Running';
  const out = [h('div', { class: 'head' }, h('div', { class: 'grow' }, h('h2', {}, v.name), status(vmStatus(v))), closeBtn('vms'))];
  out.push(h('section', {}, facts([
    ['Mac', h('a', { href: '#/macs/' + enc(v.node) }, v.node)],
    ['Guest', guestName(guestOf(req))],
    ['Image', img ? img.display_name || img.name : h('code', {}, req.image || '–')],
    ['Size', (req.vcpus || '–') + ' vCPUs, ' + gib(req.memory_mib)],
    nets.length ? ['Networks', nets.join(', ')] : null,
    isOp() ? ['Tenant', req.tenant ? h('a', { href: '#/tenants/' + enc(req.tenant) }, req.tenant) : 'none'] : null,
    ['Created', when(v.created_at)],
    v.expires_at ? ['Deleted', rel(v.expires_at) + ' (automatically)'] : null,
    ['ID', copyField(v.id, 'VM ID')],
  ])));

  const out2 = h('div', { class: 'term', 'aria-live': 'polite' }, ex.log.length ? ex.log.map((e) => [
    h('div', { class: 'cmd' }, '$ ' + e.cmd),
    e.pending ? h('div', { class: 'meta' }, 'running…') : null,
    e.stdout ? h('div', {}, e.stdout) : null,
    e.stderr ? h('div', { class: 'err' }, e.stderr) : null,
    e.error ? h('div', { class: 'err' }, e.error) : null,
    !e.pending && !e.error ? h('div', { class: 'meta' }, 'exit ' + e.exit_code) : null,
  ]) : h('div', { class: 'meta' }, 'Commands run as root through the guest agent. Output appears here.'));
  const input = h('input', { id: 'exec-input', class: 'mono', placeholder: guestOf(req) === 'macos' ? 'sw_vers' : 'uname -a',
    value: ex.draft, autocomplete: 'off', spellcheck: 'false', disabled: !running, 'aria-label': 'Command',
    oninput: (e) => { ex.draft = e.target.value; } });
  out.push(h('section', {}, h('h3', {}, 'Run a command'),
    h('form', { class: 'row', onsubmit: (e) => { e.preventDefault(); runCommand(v, ex); } },
      h('div', { class: 'grow' }, input), h('button', { type: 'submit', disabled: !running }, 'Run')),
    running ? null : h('p', { class: 'hint' }, 'The VM has to be running.'),
    out2));
  out.push(h('section', {}, h('h3', {}, 'Delete'),
    h('p', { class: 'small muted', style: { marginBottom: '10px' } }, 'Stops the VM and deletes its disk. This cannot be undone.'),
    h('button', { class: 'danger', onclick: () => deleteVm(v) }, 'Delete VM…')));
  return out;
}

async function runCommand(v, ex) {
  const cmd = ex.draft.trim();
  if (!cmd) return;
  const entry = { cmd, pending: true };
  ex.log.push(entry);
  if (ex.log.length > 20) ex.log.shift();
  ex.draft = '';
  render(true);
  try {
    const r = await api('POST', '/fleet/vms/' + enc(v.node) + '/' + enc(v.id) + '/exec', { body: { command: cmd, timeout_seconds: 60 } });
    Object.assign(entry, { stdout: r.stdout, stderr: r.stderr, exit_code: r.exit_code ?? r.exit_status ?? '?' });
  } catch (e) {
    entry.error = e.message;
  }
  entry.pending = false;
  render(true);
  const t = document.querySelector('.term');
  if (t) t.scrollTop = t.scrollHeight;
  const i = $('exec-input');
  if (i) i.focus();
}

function netInspector(n) {
  const name = enc(n.name);
  const members = Object.entries(n.members || {});
  const prefix = n.subnet.replace(/\.0\/24$/, '.');
  const candidates = (isOp() ? (S.data.nodes || []).filter((x) => x.apple).map((x) => x.name)
    : S.data.leases.filter((l) => l.state === 'active').map((l) => l.node)).filter((x) => !(x in (n.members || {})));
  const snippet = JSON.stringify({ apple: { networks: [{ name: n.name }] } });
  return [
    h('div', { class: 'head' }, h('div', { class: 'grow' }, h('h2', {}, n.name), h('span', { class: 'muted small' }, n.subnet)), closeBtn('networks')),
    h('section', {}, facts([['Subnet', h('code', {}, n.subnet)], ['MTU', String(n.mtu)], ['Port', String(n.port)],
      isOp() ? ['Tenant', n.tenant || 'Operator (not tied to leases)'] : null, ['Created', when(n.created_at)]])),
    h('section', {}, h('h3', {}, 'Macs'),
      members.length ? h('div', { class: 'stack' }, members.map(([node, [lo, hi]]) => h('div', { class: 'row' },
        h('div', { class: 'grow' }, h('div', {}, node), h('div', { class: 'small muted' }, 'Guests get ' + prefix + lo + ' to ' + prefix + hi)),
        h('button', { class: 'plain danger', onclick: () => confirmAct({
          title: 'Remove ' + node + ' from ' + n.name + '?', body: 'VMs on ' + node + ' lose their link to VMs on the other Macs.',
          confirm: 'Remove', danger: true }, 'DELETE', '/fleet/overlays/' + name + '/members/' + enc(node), node + ' removed from ' + n.name) }, 'Remove'))))
        : h('p', { class: 'muted small' }, 'No Macs yet.'),
      candidates.length ? h('form', { class: 'row', style: { marginTop: '12px' }, onsubmit: (e) => {
        e.preventDefault();
        const node = new FormData(e.target).get('node');
        doAct('POST', '/fleet/overlays/' + name + '/members', node + ' added to ' + n.name, { node });
      } }, h('select', { name: 'node', class: 'grow', style: { width: 'auto' }, 'aria-label': 'Mac to add' },
        candidates.map((c) => h('option', { value: c }, c))), h('button', { type: 'submit' }, 'Add Mac')) : null),
    h('section', {}, h('h3', {}, 'Use it'),
      h('p', { class: 'small muted' }, 'Pick ' + n.name + ' under Networks when you launch a VM, or add this to a create request:'),
      h('pre', { class: 'snippet' }, snippet),
      h('button', { class: 'plain', onclick: () => copy(snippet, 'Snippet') }, 'Copy')),
    h('section', {}, h('h3', {}, 'Encryption key'),
      h('p', { class: 'small' }, 'Key ' + n.key_epoch + ', set ' + rel(n.key_rotated_at) + '.'),
      h('p', { class: 'small muted', style: { margin: '6px 0 10px' } }, 'A new key reaches every Mac within a minute; traffic keeps flowing while they switch.'),
      h('button', { onclick: () => confirmAct({ title: 'Rotate the key for ' + n.name + '?',
        body: 'Each Mac picks up the new key within a heartbeat. The old key keeps working until the next rotation.', confirm: 'Rotate key' },
      'POST', '/fleet/overlays/' + name + '/rotate', 'New key issued for ' + n.name) }, 'Rotate key')),
    h('section', {}, h('button', { class: 'danger', onclick: () => confirmAct({ title: 'Delete ' + n.name + '?',
      body: 'VMs on it lose their links to VMs on other Macs. Their other networks keep working.', confirm: 'Delete network', danger: true, type: n.name },
    'DELETE', '/fleet/overlays/' + name, n.name + ' deleted', undefined, () => go('networks')) }, 'Delete network…')),
  ];
}

function tenantInspector(t) {
  const id = enc(t.id);
  const q = t.quota || {};
  const u = t.in_use || {};
  const leases = S.data.leases.filter((l) => l.tenant === t.id && l.state === 'active');
  const quotaForm = h('form', { class: 'stack', onsubmit: async (e) => {
    e.preventDefault();
    const quota = readQuota(new FormData(e.target));
    try { await api('PATCH', '/fleet/tenants/' + id, { body: { quota } }); toast('Quota saved for ' + t.id); refresh({ force: true }); } catch (err) { toast(err.message, true); }
  } }, quotaFields(q), h('div', { class: 'row' }, h('span', { class: 'grow' }), h('button', { type: 'submit' }, 'Save quota')));
  return [
    h('div', { class: 'head' }, h('div', { class: 'grow' }, h('h2', {}, t.display_name || t.id),
      status(t.disabled ? { text: 'Disabled', cls: 'bad' } : { text: 'Active', cls: 'ok' })), closeBtn('tenants')),
    h('section', {}, facts([['ID', h('code', {}, t.id)], ['Created', when(t.created_at)],
      ['In use', plural(u.leased_hosts || 0, 'Mac') + ', ' + plural(u.vms || 0, 'VM') + ', ' + (u.vcpus || 0) + ' vCPUs, ' + gib(u.memory_mib || 0)]])),
    h('section', {}, h('h3', {}, 'Leased Macs'), leases.length
      ? h('div', { class: 'stack' }, leases.map((l) => h('div', { class: 'row' }, h('a', { class: 'grow', href: '#/macs/' + enc(l.node) }, l.node),
        h('span', { class: 'small muted' }, leaseLine(l)))))
      : h('p', { class: 'muted small' }, 'None.'),
    h('button', { class: 'plain', style: { marginTop: '8px' }, onclick: () => leaseSheet({ tenant: t.id }) }, 'Lease a Mac to ' + t.id + '…')),
    h('section', {}, h('h3', {}, 'Quota'), h('p', { class: 'hint', style: { marginBottom: '10px' } }, 'Leave a field empty for no limit.'), quotaForm),
    h('section', {}, h('div', { class: 'actions-col' },
      h('button', { onclick: () => doAct('PATCH', '/fleet/tenants/' + id, t.id + (t.disabled ? ' enabled' : ' disabled'), { disabled: !t.disabled }) },
        t.disabled ? 'Enable tenant' : 'Disable tenant'),
      h('button', { onclick: async () => {
        if (!await confirmDialog({ title: 'Issue a new token for ' + t.id + '?', body: 'The current token stops working at once. Anyone using it has to sign in again.', confirm: 'Issue new token' })) return;
        try { const r = await api('POST', '/fleet/tenants/' + id + '/rotate-token'); tokenDialog(t.id, r.token); } catch (e) { toast(e.message, true); }
      } }, 'Issue new token…'),
      h('button', { class: 'danger', onclick: () => confirmAct({ title: 'Delete ' + t.id + '?',
        body: 'The tenant can no longer sign in. Its usage records are kept.', confirm: 'Delete tenant', danger: true, type: t.id },
      'DELETE', '/fleet/tenants/' + id, t.id + ' deleted', undefined, () => go('tenants')) }, 'Delete tenant…'))),
  ];
}

function quotaFields(q = {}) {
  const f = (name, label, value, hint) => h('label', { class: 'field' }, h('span', {}, label),
    h('input', { name, type: 'number', min: '0', inputmode: 'numeric', value: value === undefined || value === null ? '' : String(value), placeholder: 'No limit' }),
    hint ? h('div', { class: 'hint' }, hint) : null);
  return h('div', { class: 'grid2' },
    f('max_leased_hosts', 'Macs', q.max_leased_hosts),
    f('max_vms', 'VMs', q.max_vms),
    f('max_vcpus', 'vCPUs', q.max_vcpus),
    f('max_memory_gib', 'Memory (GiB)', q.max_memory_mib ? q.max_memory_mib / 1024 : undefined));
}

function readQuota(f) {
  const n = (k) => { const v = String(f.get(k) || '').trim(); return v === '' ? undefined : Number(v); };
  const quota = {};
  for (const k of ['max_leased_hosts', 'max_vms', 'max_vcpus']) if (n(k) !== undefined) quota[k] = n(k);
  if (n('max_memory_gib') !== undefined) quota.max_memory_mib = Math.round(n('max_memory_gib') * 1024);
  return quota;
}

// ---- actions --------------------------------------------------------------------------------------------------------

async function doAct(method, path, done, body, after) {
  try {
    await api(method, path, body === undefined ? {} : { body });
    if (done) toast(done);
    if (after) after();
  } catch (e) { toast(e.message, true); }
  refresh({ force: true });
}

async function confirmAct(opts, method, path, done, body, after) {
  if (await confirmDialog(opts)) await doAct(method, path, done, body, after);
}

async function releaseLease(l, m) {
  const { releasable } = leaseProgress(l);
  const force = isOp() && !releasable;
  const vms = m ? m.vms.length : 0;
  await confirmAct({
    title: (force ? 'Force-release ' : 'Release ') + l.node + '?',
    body: (vms ? plural(vms, 'VM') + ' on it ' + (vms === 1 ? 'is' : 'are') + ' deleted. ' : '')
      + (force ? 'The lease is still billed until ' + when(l.min_end) + '. ' : '')
      + 'The Mac is scrubbed before anyone else can lease it.',
    confirm: force ? 'Force release' : 'Release Mac', danger: true, type: vms ? l.node : null,
  }, 'POST', '/fleet/leases/' + enc(l.id) + '/release' + (force ? '?force=true' : ''), l.node + ' released');
}

async function scrubMac(m) {
  await confirmAct({ title: 'Scrub ' + m.name + '?',
    body: 'Every VM left on it is deleted with its disks, and the Mac can be leased again. Do your host-level wipe first (Keychain, caches) if your runbook needs one.',
    confirm: 'Scrub Mac', danger: true, type: m.vms.length ? m.name : null },
  'POST', '/fleet/nodes/' + enc(m.name) + '/scrub?force=true', m.name + ' scrubbed and available');
}

async function deleteVm(v) {
  await confirmAct({ title: 'Delete ' + v.name + '?', body: 'The VM stops and its disk is deleted. This cannot be undone.',
    confirm: 'Delete VM', danger: true, type: v.name },
  'DELETE', '/fleet/vms/' + enc(v.node) + '/' + enc(v.id), v.name + ' deleted', undefined, () => go('vms'));
}

async function removeImage(i) {
  await confirmAct({ title: 'Remove ' + (i.display_name || i.name) + ' from the catalog?',
    body: 'VMs already made from it keep running, and the disk stays on the Macs. Tenants can no longer pick it.', confirm: 'Remove image', danger: true },
  'DELETE', '/fleet/images/' + enc(i.name), (i.display_name || i.name) + ' removed from the catalog');
}

// ---- dialogs --------------------------------------------------------------------------------------------------------

/** A modal sheet. `onSubmit(formData, form)` throws to show an error inline; resolves to close. */
function openSheet({ title, intro, body, submit, danger, onSubmit, cancel = 'Cancel', wide }) {
  const dlg = $('sheet');
  const err = h('p', { class: 'sheet-error', role: 'alert' });
  const btn = h('button', { class: danger ? 'danger solid' : 'primary', type: 'submit', value: 'ok' }, submit);
  const form = h('form', { method: 'dialog', novalidate: false, onsubmit: async (e) => {
    e.preventDefault();
    if (!form.reportValidity()) return;
    btn.disabled = true;
    err.textContent = '';
    const label = btn.textContent;
    btn.textContent = label.replace(/…$/, '') + '…';
    try {
      await onSubmit(new FormData(form), form);
      dlg.close();
    } catch (x) {
      err.textContent = x.message || String(x);
      btn.disabled = false;
      btn.textContent = label;
    }
  } },
  h('div', { class: 'sheet-head' }, h('h2', {}, title), intro ? h('p', {}, intro) : null),
  h('div', { class: 'sheet-body' }, body, err),
  h('div', { class: 'sheet-foot' }, cancel ? h('button', { type: 'button', onclick: () => dlg.close() }, cancel) : null, btn));
  dlg.style.width = wide ? 'min(680px, calc(100vw - 32px))' : '';
  dlg.replaceChildren(form);
  if (!dlg.open) dlg.showModal();
  const first = form.querySelector('input:not([type=hidden]):not([type=radio]):not([type=checkbox]), select, textarea');
  if (first) first.focus();
  return { dlg, form, btn, err };
}

function confirmDialog({ title, body, confirm, danger, type }) {
  return new Promise((resolve) => {
    let ok = false;
    const typed = type ? h('input', { autocomplete: 'off', spellcheck: 'false', 'aria-label': 'Type ' + type + ' to confirm', required: true }) : null;
    const { dlg, btn } = openSheet({
      title, submit: confirm, danger,
      body: [h('p', {}, body), typed ? h('label', { class: 'field' }, h('span', {}, 'Type ', h('b', {}, type), ' to confirm'), typed) : null],
      onSubmit: async () => { ok = true; },
    });
    if (typed) {
      btn.disabled = true;
      typed.addEventListener('input', () => { btn.disabled = typed.value !== type; });
      typed.focus();
    } else btn.focus();
    dlg.addEventListener('close', () => resolve(ok), { once: true });
  });
}

function tokenDialog(id, tok) {
  openSheet({
    title: 'Token for ' + id,
    intro: 'Send it to the tenant over a private channel. This is the only time it is shown: central keeps only its hash.',
    body: [h('div', { class: 'token-box' }, tok), h('button', { type: 'button', onclick: () => copy(tok, 'Token') }, 'Copy token'),
      h('p', { class: 'small muted' }, 'The tenant signs in at ' + location.origin + '/console with it.')],
    submit: 'Done', cancel: null, onSubmit: async () => {},
  });
}

function choice(name, value, checked, title, sub, disabled) {
  return h('label', { class: 'choice' }, h('input', { type: 'radio', name, value, checked: !!checked, disabled: !!disabled }),
    h('b', {}, title), sub ? h('small', {}, sub) : null);
}

function leaseSheet(pre = {}) {
  const op = isOp();
  const avail = op ? macs().filter((m) => m.isMac && !m.lease && !m.vms.length && !m.node.cordoned && !m.drain.scrub_pending && m.node.healthy !== false) : [];
  const tenants = op ? (S.data.tenants || []).filter((t) => !t.disabled) : [];
  if (op && !tenants.length) {
    toast('Create a tenant first: a lease belongs to a tenant', true);
    go('tenants');
    return;
  }
  const body = [];
  if (op) {
    body.push(h('div', { class: 'grid2' },
      h('label', { class: 'field' }, h('span', {}, 'Tenant'), h('select', { name: 'tenant', required: true },
        tenants.map((t) => h('option', { value: t.id, selected: t.id === pre.tenant }, t.display_name || t.id)))),
      h('label', { class: 'field' }, h('span', {}, 'Mac'), h('select', { name: 'node' },
        h('option', { value: '' }, 'Smallest available'),
        avail.map((m) => h('option', { value: m.name, selected: m.name === pre.node }, m.name + ' (' + (m.node.vcpus_total || '?') + ' vCPUs, ' + gib(m.node.memory_mib_total) + ')'))))));
    if (!avail.length) body.push(h('p', { class: 'note warn' }, 'No Mac is free right now: each one is leased, busy, cordoned or waiting for a scrub.'));
  } else {
    body.push(h('div', {}, h('span', { class: 'field-label' }, 'Size'), h('div', { class: 'choices' },
      choice('size', 'any', true, 'Any Mac', 'The smallest one free'),
      choice('size', 'm', false, '8+ vCPUs', '24 GiB memory or more'),
      choice('size', 'l', false, '16+ vCPUs', '48 GiB memory or more'))));
    body.push(check('nested', false, 'Needs nested virtualization', 'For VMs that run their own VMs (an M3 Mac or later).'));
  }
  body.push(h('div', {}, h('span', { class: 'field-label' }, 'How long'), h('div', { class: 'choices' },
    choice('hours', '24', true, '24 hours', 'The minimum'),
    choice('hours', '72', false, '3 days'),
    choice('hours', '168', false, '1 week'),
    choice('hours', '', false, 'Until released', 'At least 24 hours'))));
  body.push(h('p', { class: 'note' }, 'A lease is billed for at least 24 hours, even if it is released sooner. Apple\'s macOS licence requires it.'));
  openSheet({
    title: op ? 'Lease a Mac to a tenant' : 'Lease a Mac',
    intro: op ? null : 'The Mac is yours alone: only your VMs run on it, up to two of them macOS.',
    body, submit: 'Lease Mac',
    onSubmit: async (f) => {
      const b = {};
      if (f.get('hours')) b.hours = Number(f.get('hours'));
      if (op) {
        b.tenant = f.get('tenant');
        if (f.get('node')) b.node = f.get('node');
      } else {
        const size = f.get('size');
        if (size === 'm') { b.min_vcpus = 8; b.min_memory_mib = 24 * 1024; }
        if (size === 'l') { b.min_vcpus = 16; b.min_memory_mib = 48 * 1024; }
        if (f.has('nested')) b.nested_virtualization = true;
      }
      const r = await api('POST', '/fleet/leases', { body: b });
      const node = r.lease && r.lease.node;
      toast('Leased ' + node + (op ? ' to ' + b.tenant : ''));
      await refresh({ force: true });
      if (node) go('macs', node);
    },
  });
}

function launchSheet(pre = {}) {
  const op = isOp();
  const images = S.data.images;
  const leased = macs().filter((m) => m.lease);
  const targets = op ? macs().filter((m) => m.node.healthy !== false && !m.node.cordoned) : leased;
  const q = op ? {} : S.me.quota || {};
  const u = op ? {} : S.me.in_use || {};
  const left = (max, used) => (max === undefined || max === null ? null : Math.max(0, max - used));
  const leftVms = left(q.max_vms, u.vms || 0);
  const leftCpu = left(q.max_vcpus, u.vcpus || 0);
  const leftMem = left(q.max_memory_mib, u.memory_mib || 0);

  const initial = images.find((i) => i.name === pre.image) || images[0];
  const imgChoices = h('div', { class: 'choices' },
    images.map((i) => choice('image', i.name, initial === i, i.display_name || i.name, guestName(i.guest_os) + ', ' + i.vcpus + ' vCPUs, ' + gib(i.memory_mib))),
    choice('image', '__custom', !initial, 'Custom disk', 'A disk path on the Mac'));
  const custom = h('div', { class: 'grid2' },
    h('label', { class: 'field' }, h('span', {}, 'Disk path on the Mac'), h('input', { name: 'custom_image', placeholder: '/Users/fluxvm/vms/macos/disk.raw' })),
    h('label', { class: 'field' }, h('span', {}, 'Guest'), h('select', { name: 'custom_guest' }, h('option', { value: 'macos' }, 'macOS'), h('option', { value: 'linux' }, 'Linux'))));
  const nameIn = h('input', { name: 'name', required: true, pattern: '[A-Za-z0-9][A-Za-z0-9._\\-]{0,62}', autocomplete: 'off', spellcheck: 'false' });
  const cpuIn = h('input', { name: 'vcpus', type: 'number', min: '1', max: '64', required: true });
  const memIn = h('input', { name: 'memory_gib', type: 'number', min: '1', step: '1', required: true });
  const macSel = h('select', { name: 'node' });
  const notice = h('div');
  const summary = h('p', { class: 'small muted' });
  const nets = S.data.overlays;

  const current = () => {
    const f = new FormData(formRef.form);
    const iname = f.get('image');
    const img = images.find((i) => i.name === iname);
    return { img, guest: img ? img.guest_os : f.get('custom_guest'), custom: iname === '__custom' };
  };
  const uniqueName = (base) => {
    const taken = new Set(S.data.vms.map((v) => v.name));
    for (let i = 1; ; i++) if (!taken.has(base + '-' + i)) return base + '-' + i;
  };
  let touchedName = false;
  nameIn.addEventListener('input', () => { touchedName = true; });

  const update = (fromImage) => {
    const { img, guest, custom: isCustom } = current();
    custom.hidden = !isCustom;
    custom.querySelector('input').required = isCustom;
    if (fromImage) {
      if (img) { cpuIn.value = img.vcpus; memIn.value = Math.round(img.memory_mib / 1024); } else if (!cpuIn.value) { cpuIn.value = 4; memIn.value = 8; }
      if (!touchedName) nameIn.value = uniqueName(img ? img.name.replace(/[^a-z0-9-]/g, '-') : 'vm');
    }
    // Which Macs can take it
    const prev = macSel.value || pre.node || '';
    const opts = [h('option', { value: '' }, 'Choose automatically')];
    for (const m of targets) {
      const full = guest === 'macos' && m.isMac && m.macosGuests >= 2;
      const notMac = guest === 'macos' && !m.isMac;
      const free = m.node.free_vcpus !== undefined ? ', ' + m.node.free_vcpus + ' vCPUs free' : '';
      const slots = m.isMac ? (2 - Math.min(2, m.macosGuests)) + ' of 2 macOS slots free' : 'Linux host';
      opts.push(h('option', { value: m.name, disabled: full || notMac, selected: m.name === prev },
        m.name + ' (' + (full ? 'no macOS slot free' : slots + free) + ')'));
    }
    macSel.replaceChildren(...opts);
    // What will stop it
    const msgs = [];
    if (!op && guest === 'macos' && !leased.length) {
      msgs.push(h('div', { class: 'note warn row' }, h('span', { class: 'grow' }, 'macOS VMs run on Macs you lease. Lease one first.'),
        h('button', { type: 'button', onclick: () => { $('sheet').close(); leaseSheet(); } }, 'Lease a Mac')));
    } else if (guest === 'macos' && targets.length && targets.every((m) => !m.isMac || m.macosGuests >= 2)) {
      msgs.push(h('p', { class: 'note warn' }, 'Every ' + (op ? '' : 'leased ') + 'Mac already runs two macOS VMs, Apple\'s limit. Delete one or lease another Mac.'));
    }
    if (leftVms === 0) msgs.push(h('p', { class: 'note warn' }, 'You have used your quota of ' + q.max_vms + ' VMs.'));
    notice.replaceChildren(...msgs);
    const c = Number(cpuIn.value || 0);
    const mem = Number(memIn.value || 0) * 1024;
    const parts = [];
    if (leftCpu !== null) parts.push(c + ' of your ' + leftCpu + ' remaining vCPUs');
    if (leftMem !== null) parts.push(gib(mem) + ' of your ' + gib(leftMem) + ' remaining memory');
    summary.textContent = parts.length ? 'Uses ' + parts.join(' and ') + '.' : '';
    summary.style.color = (leftCpu !== null && c > leftCpu) || (leftMem !== null && mem > leftMem) ? 'var(--bad)' : '';
  };

  const body = [
    h('div', {}, h('span', { class: 'field-label' }, 'Image'), imgChoices),
    custom,
    h('label', { class: 'field' }, h('span', {}, 'Name'), nameIn),
    h('div', { class: 'grid2' }, h('label', { class: 'field' }, h('span', {}, 'vCPUs'), cpuIn), h('label', { class: 'field' }, h('span', {}, 'Memory (GiB)'), memIn)),
    summary,
    h('label', { class: 'field' }, h('span', {}, 'Mac'), macSel),
    nets.length ? h('div', {}, h('span', { class: 'field-label' }, 'Private networks'),
      nets.map((n) => h('label', { class: 'check' }, h('input', { type: 'checkbox', name: 'net', value: n.name }),
        h('span', {}, n.name, h('span', { class: 'hint', style: { display: 'block' } }, n.subnet + ' on ' + (Object.keys(n.members || {}).join(', ') || 'no Macs yet')))))) : null,
    h('label', { class: 'field' }, h('span', {}, 'Delete automatically'), h('select', { name: 'ttl' },
      h('option', { value: '' }, 'Never'), h('option', { value: '3600' }, 'After 1 hour'), h('option', { value: '28800' }, 'After 8 hours'),
      h('option', { value: '86400' }, 'After 24 hours'), h('option', { value: '604800' }, 'After 7 days')),
    h('div', { class: 'hint' }, 'Handy for test and CI machines. Automatic deletion also lets an operator drain a Mac without asking you.')),
    notice,
  ];
  const formRef = openSheet({
    title: 'Launch VM', body, submit: 'Launch VM', wide: true,
    onSubmit: async (f) => {
      const { img, guest, custom: isCustom } = current();
      const req = {
        name: String(f.get('name')).trim(),
        backend: 'vz',
        image: isCustom ? String(f.get('custom_image')).trim() : img.image,
        vcpus: Number(f.get('vcpus')),
        memory_mib: Math.round(Number(f.get('memory_gib')) * 1024),
        apple: { guest_os: guest },
      };
      if (img && img.disk_size_gib) req.disk_size_gib = img.disk_size_gib;
      const netNames = f.getAll('net');
      if (netNames.length) req.apple.networks = netNames.map((name) => ({ name }));
      if (f.get('ttl')) req.ttl_seconds = Number(f.get('ttl'));
      if (f.get('node')) req.node = f.get('node');
      const r = await api('POST', '/fleet/vms', { body: req });
      toast('Launched ' + req.name + ' on ' + r.node);
      await refresh({ force: true });
      if (r.vm && r.vm.id) go('vms', r.node + '/' + r.vm.id);
    },
  });
  formRef.form.addEventListener('change', (e) => update(e.target.name === 'image'));
  formRef.form.addEventListener('input', (e) => { if (e.target === cpuIn || e.target === memIn) update(false); });
  update(true);
  if (pre.node) { macSel.value = pre.node; }
}

function networkSheet() {
  const op = isOp();
  const leased = S.data.leases.filter((l) => l.state === 'active').map((l) => l.node);
  const nodes = op ? (S.data.nodes || []).filter((n) => n.apple).map((n) => n.name) : leased;
  if (!op && !leased.length) {
    toast('Lease a Mac first: a private network connects the Macs you lease', true);
    return;
  }
  const body = [
    h('label', { class: 'field' }, h('span', {}, 'Name'), h('input', { name: 'name', required: true, pattern: '[a-z0-9][a-z0-9\\-]{0,30}', placeholder: 'backend', autocomplete: 'off' }),
      h('div', { class: 'hint' }, 'Lowercase letters, digits and dashes. VMs join it by this name.')),
    op ? h('label', { class: 'field' }, h('span', {}, 'Tenant'), h('select', { name: 'tenant' },
      h('option', { value: '' }, 'None (an operator network)'),
      (S.data.tenants || []).map((t) => h('option', { value: t.id }, t.display_name || t.id))),
    h('div', { class: 'hint' }, 'A tenant network follows the tenant\'s leases. An operator network is open to any VM on its Macs.')) : null,
    h('div', {}, h('span', { class: 'field-label' }, 'Macs'), op ? h('p', { class: 'hint', style: { marginTop: 0 } }, 'For a tenant network, leave these unticked to use every Mac the tenant leases.') : null,
      nodes.map((n) => h('label', { class: 'check' }, h('input', { type: 'checkbox', name: 'node', value: n, checked: !op }), h('span', {}, n)))),
    h('details', { class: 'adv' }, h('summary', {}, 'Advanced'), h('div', { class: 'grid2' },
      h('label', { class: 'field' }, h('span', {}, 'VMs per Mac'), h('input', { name: 'hosts_per_node', type: 'number', min: '2', max: '126', value: '30' }),
        h('div', { class: 'hint' }, 'Addresses each Mac gets on this network.')),
      h('label', { class: 'field' }, h('span', {}, 'MTU'), h('input', { name: 'mtu', type: 'number', min: '576', max: '9000', value: '1400' }),
        h('div', { class: 'hint' }, 'Raise it only on a jumbo-frame network.')))),
  ];
  openSheet({
    title: 'Create a private network', intro: 'VMs on it reach each other by IP across Macs. Traffic between Macs is encrypted.',
    body, submit: 'Create network',
    onSubmit: async (f) => {
      const b = { name: f.get('name'), hosts_per_node: Number(f.get('hosts_per_node')), mtu: Number(f.get('mtu')) };
      const picked = f.getAll('node');
      if (picked.length) b.nodes = picked;
      if (op && f.get('tenant')) b.tenant = f.get('tenant');
      const n = await api('POST', '/fleet/overlays', { body: b });
      toast('Created ' + n.name + ' (' + n.subnet + ')');
      await refresh({ force: true });
      go('networks', n.name);
    },
  });
}

function tenantSheet() {
  openSheet({
    title: 'Create a tenant', intro: 'You get the tenant\'s token once, on the next screen.',
    body: [
      h('div', { class: 'grid2' },
        h('label', { class: 'field' }, h('span', {}, 'ID'), h('input', { name: 'id', required: true, pattern: '[a-z0-9][a-z0-9\\-]{0,62}', placeholder: 'acme', autocomplete: 'off' }),
          h('div', { class: 'hint' }, 'Lowercase, used in URLs and network names.')),
        h('label', { class: 'field' }, h('span', {}, 'Display name'), h('input', { name: 'display_name', placeholder: 'Acme Inc.' }))),
      h('div', {}, h('span', { class: 'field-label' }, 'Quota'), h('p', { class: 'hint', style: { margin: '0 0 8px' } }, 'Leave a field empty for no limit.'),
        quotaFields({ max_leased_hosts: 2 })),
    ],
    submit: 'Create tenant',
    onSubmit: async (f) => {
      const b = { id: String(f.get('id')).trim(), quota: readQuota(f) };
      const dn = String(f.get('display_name') || '').trim();
      if (dn) b.display_name = dn;
      const r = await api('POST', '/fleet/tenants', { body: b });
      refresh({ force: true });
      setTimeout(() => { tokenDialog(b.id, r.token); go('tenants', b.id); }, 0);
    },
  });
}

function imageSheet(i) {
  const editing = !!i;
  i = i || { guest_os: 'macos', vcpus: 4, memory_mib: 8192 };
  openSheet({
    title: editing ? 'Edit ' + (i.display_name || i.name) : 'Add an image',
    intro: 'The disk must already be at this path on every Mac that should run it. fluxctl vm-image pull puts it there.',
    body: [
      h('div', { class: 'grid2' },
        h('label', { class: 'field' }, h('span', {}, 'ID'), h('input', { name: 'name', required: true, value: i.name || '', readonly: editing, pattern: '[a-z0-9][a-z0-9._\\-]{0,62}', placeholder: 'macos-27-xcode' })),
        h('label', { class: 'field' }, h('span', {}, 'Display name'), h('input', { name: 'display_name', value: i.display_name || '', placeholder: 'macOS 27 with Xcode' }))),
      h('div', { class: 'grid2' },
        h('label', { class: 'field' }, h('span', {}, 'Guest'), h('select', { name: 'guest_os' },
          h('option', { value: 'macos', selected: i.guest_os === 'macos' }, 'macOS'), h('option', { value: 'linux', selected: i.guest_os === 'linux' }, 'Linux'))),
        h('label', { class: 'field' }, h('span', {}, 'Source (optional)'), h('input', { name: 'source', value: i.source || '', placeholder: 'ghcr.io/acme/macos-xcode:27' }))),
      h('label', { class: 'field' }, h('span', {}, 'Disk path on the Macs'), h('input', { name: 'image', required: true, value: i.image || '', placeholder: '/Users/fluxvm/vms/macos-27-xcode/disk.raw', class: 'mono' })),
      h('div', { class: 'grid2' },
        h('label', { class: 'field' }, h('span', {}, 'Default vCPUs'), h('input', { name: 'vcpus', type: 'number', min: '1', max: '64', value: String(i.vcpus), required: true })),
        h('label', { class: 'field' }, h('span', {}, 'Default memory (GiB)'), h('input', { name: 'memory_gib', type: 'number', min: '1', value: String(Math.round(i.memory_mib / 1024)), required: true }))),
      h('label', { class: 'field' }, h('span', {}, 'Description (optional)'), h('input', { name: 'description', value: i.description || '', placeholder: 'Xcode 27, Homebrew and the GitHub runner' })),
    ],
    submit: editing ? 'Save changes' : 'Add image',
    onSubmit: async (f) => {
      const s = (k) => { const v = String(f.get(k) || '').trim(); return v || undefined; };
      const body = { guest_os: f.get('guest_os'), image: s('image'), vcpus: Number(f.get('vcpus')),
        memory_mib: Math.round(Number(f.get('memory_gib')) * 1024), display_name: s('display_name'), description: s('description'),
        source: s('source'), disk_size_gib: i.disk_size_gib };
      const r = await api('PUT', '/fleet/images/' + enc(s('name')), { body });
      toast((editing ? 'Saved ' : 'Added ') + (r.display_name || r.name));
      refresh({ force: true });
    },
  });
}

function drainSheet(m) {
  const leased = !!m.lease;
  openSheet({
    title: 'Drain ' + m.name, intro: 'Draining cordons the Mac first, so nothing new is placed on it. Choose what happens to the ' + plural(m.vms.length, 'VM') + ' on it.',
    body: [
      h('div', { class: 'choices', style: { gridTemplateColumns: '1fr' } },
        choice('mode', 'cordon', true, 'Leave them running', 'Only stop new VMs. Wait for them to finish, then uncordon.'),
        choice('mode', 'evict-ephemeral', false, 'Delete temporary VMs', 'VMs with an automatic deletion time, or labelled fluxvm.dev/ephemeral=true. Others keep running.'),
        choice('mode', 'reschedule', false, 'Move what can be moved', 'VMs created as reschedulable are recreated on another Mac from their spec (their disks do not move). Temporary VMs are deleted.')),
      leased ? h('div', {}, h('p', { class: 'note warn' }, m.name + ' is leased to ' + m.lease.tenant + '. Draining it breaks their sole use of the Mac.'),
        check('force', false, 'Drain anyway', 'Tell the tenant first.')) : null,
    ],
    submit: 'Drain Mac', danger: true,
    onSubmit: async (f) => {
      if (leased && !f.has('force')) throw new Error('Tick “Drain anyway” to drain a leased Mac.');
      const r = await api('POST', '/fleet/nodes/' + enc(m.name) + '/drain?mode=' + enc(f.get('mode')) + (f.has('force') ? '&force=true' : ''));
      const n = (k) => (Array.isArray(r[k]) ? r[k].length : 0);
      toast(m.name + ' draining: ' + [n('deleted') && n('deleted') + ' deleted', n('rescheduled') && n('rescheduled') + ' moved', n('kept') && n('kept') + ' kept running']
        .filter(Boolean).join(', ') || m.name + ' cordoned');
      refresh({ force: true });
    },
  });
}

// ---- toasts ---------------------------------------------------------------------------------------------------------

function toast(msg, bad) {
  const t = h('div', { class: 'toast' + (bad ? ' bad' : '') }, msg);
  $('toasts').append(t);
  setTimeout(() => t.remove(), bad ? 7000 : 3500);
}

// ---- command palette ------------------------------------------------------------------------------------------------

let palItems = [];
let palSel = 0;
function paletteCommands() {
  const cmds = [];
  for (const p of PAGES()) if (!p.sep) cmds.push({ label: 'Go to ' + p.label, hint: 'Page', run: () => go(p.id) });
  cmds.push({ label: 'Launch VM', hint: 'Action', run: () => launchSheet() });
  cmds.push({ label: isOp() ? 'Lease a Mac to a tenant' : 'Lease a Mac', hint: 'Action', run: () => leaseSheet() });
  cmds.push({ label: 'Create network', hint: 'Action', run: networkSheet });
  if (isOp()) {
    cmds.push({ label: 'Create tenant', hint: 'Action', run: tenantSheet });
    cmds.push({ label: 'Add image', hint: 'Action', run: () => imageSheet() });
    cmds.push({ label: 'Reconcile now', hint: 'Action', run: reconcileNow });
  }
  cmds.push({ label: 'Switch theme', hint: 'Action', run: cycleTheme });
  for (const v of S.data.vms) cmds.push({ label: v.name, hint: 'VM on ' + v.node, run: () => go('vms', vmKey(v)) });
  for (const m of macs()) cmds.push({ label: m.name, hint: 'Mac', run: () => go('macs', m.name) });
  for (const n of S.data.overlays) cmds.push({ label: n.name, hint: 'Network', run: () => go('networks', n.name) });
  for (const t of S.data.tenants || []) cmds.push({ label: t.display_name || t.id, hint: 'Tenant', run: () => go('tenants', t.id) });
  return cmds;
}

function openPalette() {
  if (!S.data || $('sheet').open) return;
  $('palette-input').value = '';
  drawPalette();
  $('palette').showModal();
  $('palette-input').focus();
}

function drawPalette() {
  const q = $('palette-input').value.trim().toLowerCase();
  palItems = paletteCommands().filter((c) => !q || (c.label + ' ' + c.hint).toLowerCase().includes(q)).slice(0, 40);
  palSel = Math.min(palSel, Math.max(0, palItems.length - 1));
  $('palette-list').replaceChildren(...(palItems.length ? palItems.map((c, i) => h('li', { role: 'option', 'aria-selected': i === palSel ? 'true' : 'false',
    onclick: () => runPalette(i), onmousemove: () => { if (palSel !== i) { palSel = i; drawPalette(); } } },
  h('span', { class: 'grow' }, c.label), h('span', { class: 'muted small' }, c.hint)))
    : [h('li', { class: 'muted' }, 'Nothing matches.')]));
}

function runPalette(i) {
  const c = palItems[i];
  $('palette').close();
  if (c) c.run();
}

$('palette-input').addEventListener('input', () => { palSel = 0; drawPalette(); });
$('palette-input').addEventListener('keydown', (e) => {
  if (e.key === 'ArrowDown') { palSel = Math.min(palItems.length - 1, palSel + 1); drawPalette(); e.preventDefault(); }
  if (e.key === 'ArrowUp') { palSel = Math.max(0, palSel - 1); drawPalette(); e.preventDefault(); }
  if (e.key === 'Enter') { runPalette(palSel); e.preventDefault(); }
});
$('palette').addEventListener('click', (e) => { if (e.target === $('palette')) $('palette').close(); });
$('sheet').addEventListener('click', (e) => { if (e.target === $('sheet')) $('sheet').close(); });

document.addEventListener('keydown', (e) => {
  const typing = /^(INPUT|TEXTAREA|SELECT)$/.test(document.activeElement.tagName);
  if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') { e.preventDefault(); openPalette(); return; }
  if (typing || $('sheet').open || $('palette').open) return;
  if (e.key === '/') { e.preventDefault(); $('search').focus(); }
  if (e.key === 'Escape' && route().id) go(route().page);
});

// ---- chrome ---------------------------------------------------------------------------------------------------------

function applyTheme() {
  const t = storage.get('localStorage', THEME_KEY) || 'system';
  if (t === 'system') document.documentElement.removeAttribute('data-theme');
  else document.documentElement.setAttribute('data-theme', t);
  $('theme').textContent = { system: 'Auto', light: 'Light', dark: 'Dark' }[t];
  $('theme').title = 'Theme: ' + t + '. Click to change.';
}
function cycleTheme() {
  const order = ['system', 'light', 'dark'];
  const cur = storage.get('localStorage', THEME_KEY) || 'system';
  storage.set('localStorage', THEME_KEY, order[(order.indexOf(cur) + 1) % 3]);
  applyTheme();
}

$('theme').addEventListener('click', cycleTheme);
$('refresh').addEventListener('click', () => refresh({ force: true }));
$('signout').addEventListener('click', () => {
  storage.del('sessionStorage', TOKEN_KEY);
  location.hash = '';
  location.reload();
});
$('search').addEventListener('input', (e) => {
  S.query = e.target.value;
  if (route().page !== 'vms') go('vms'); else render();
});
$('signin-form').addEventListener('submit', (e) => {
  e.preventDefault();
  const t = $('signin-token').value.trim();
  if (!t) return;
  storage.set('sessionStorage', TOKEN_KEY, t);
  $('signin-token').value = '';
  S.role = null;
  S.data = null;
  refresh({ force: true });
});
window.addEventListener('scroll', () => $('bar').classList.toggle('scrolled', window.scrollY > 4), { passive: true });

applyTheme();
refresh({ force: true });
