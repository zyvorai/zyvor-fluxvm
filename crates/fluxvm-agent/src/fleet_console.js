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
    else if (k === 'style' && typeof v === 'object') {
      // Custom properties (`--p`) are not plain properties of CSSStyleDeclaration: they need setProperty.
      for (const [p, x] of Object.entries(v)) if (p.startsWith('--')) e.style.setProperty(p, x); else e.style[p] = x;
    }
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
// A small inline icon set (24px grid, 1.75 stroke, currentColor), so the console needs no icon font or asset.
const ICONS = {
  bell: 'M6 8a6 6 0 0 1 12 0c0 7 3 9 3 9H3s3-2 3-9M10.3 21a1.94 1.94 0 0 0 3.4 0',
  refresh: 'M21 12a9 9 0 1 1-3-6.7L21 8M21 3v5h-5',
  sun: 'M12 8a4 4 0 1 0 0 8 4 4 0 0 0 0-8M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4',
  moon: 'M12 3a6 6 0 0 0 9 9 9 9 0 1 1-9-9Z',
  auto: 'M4 5h16a1 1 0 0 1 1 1v10a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1V6a1 1 0 0 1 1-1M8 21h8M12 17v4',
  menu: 'M4 6h16M4 12h16M4 18h16',
  x: 'M18 6 6 18M6 6l12 12',
  check: 'M20 6 9 17l-5-5',
  alert: 'M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0ZM12 9v4M12 17h.01',
  info: 'M12 2a10 10 0 1 0 0 20 10 10 0 0 0 0-20M12 16v-4M12 8h.01',
  chev: 'm6 9 6 6 6-6',
  plus: 'M12 5v14M5 12h14',
  home: 'M3 11 12 3l9 8M5 10v10h5v-6h4v6h5V10',
  mac: 'M5 4h14a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2M8 21h8M12 17v4',
  vm: 'M21 8 12 3 3 8v8l9 5 9-5V8ZM3 8l9 5 9-5M12 13v8',
  network: 'M9 2h6v6H9zM2 16h6v6H2zM16 16h6v6h-6zM5 16v-3a1 1 0 0 1 1-1h12a1 1 0 0 1 1 1v3M12 12V8',
  disk: 'M22 12H2M5.5 5.1 2 12v6a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-6l-3.5-6.9A2 2 0 0 0 16.7 4H7.3a2 2 0 0 0-1.8 1.1ZM6 16h.01M10 16h.01',
  image: 'M5 3h14a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2M9 8.5a.5.5 0 1 0 0 1 .5.5 0 0 0 0-1M21 15l-3.1-3.1a2 2 0 0 0-2.8 0L6 21',
  zap: 'M13 2 3 14h9l-1 8 10-12h-9l1-8Z',
  users: 'M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2M9 3a4 4 0 1 0 0 8 4 4 0 0 0 0-8M22 21v-2a4 4 0 0 0-3-3.9M16 3.1a4 4 0 0 1 0 7.8',
  card: 'M4 5h16a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V7a2 2 0 0 1 2-2M2 10h20',
  chart: 'M3 3v18h18M7 16v-5M12 16V7M17 16v-8',
  pulse: 'M22 12h-4l-3 9L9 3l-3 9H2',
  term: 'm4 17 6-6-6-6M12 19h8',
  key: 'M7.5 11a4.5 4.5 0 1 0 0 9 4.5 4.5 0 0 0 0-9M10.7 12.3 21 2M16 7l3 3M14 9l2 2',
  shield: 'M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10Z',
  building: 'M6 2h12a2 2 0 0 1 2 2v18H4V4a2 2 0 0 1 2-2M9 22v-4h6v4M8 6h.01M12 6h.01M16 6h.01M8 10h.01M12 10h.01M16 10h.01M8 14h.01M12 14h.01M16 14h.01',
  bellring: 'M6 8a6 6 0 0 1 12 0c0 7 3 9 3 9H3s3-2 3-9M10.3 21a1.94 1.94 0 0 0 3.4 0',
};
function icon(name, size) {
  const NS = 'http://www.w3.org/2000/svg';
  const svg = document.createElementNS(NS, 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('fill', 'none');
  svg.setAttribute('stroke', 'currentColor');
  svg.setAttribute('stroke-width', '1.75');
  svg.setAttribute('stroke-linecap', 'round');
  svg.setAttribute('stroke-linejoin', 'round');
  svg.setAttribute('aria-hidden', 'true');
  svg.setAttribute('width', String(size || 18));
  svg.setAttribute('height', String(size || 18));
  svg.classList.add('ico');
  const path = document.createElementNS(NS, 'path');
  path.setAttribute('d', ICONS[name] || ICONS.info);
  svg.append(path);
  return svg;
}

const token = () => storage.get('sessionStorage', TOKEN_KEY);
const enc = encodeURIComponent;

const gib = (mib) => (mib === undefined || mib === null ? '–'
  : (mib / 1024).toFixed(mib % 1024 ? 1 : 0).replace(/\.0$/, '') + ' GiB');
/** An amount in the price list's currency. */
function money(n, cur, price) {
  const c = cur || (S.data && S.data.prices && S.data.prices.currency) || 'USD';
  // Unit prices keep their small fractions ($0.004 per GiB-hour); amounts round to cents.
  const opts = price ? { style: 'currency', currency: c, maximumFractionDigits: 4 } : { style: 'currency', currency: c };
  try { return new Intl.NumberFormat(undefined, opts).format(n || 0); } catch (_) { return (n || 0).toFixed(price ? 4 : 2) + ' ' + c; }
}
const pricesSet = () => !!(S.data && S.data.bill && S.data.bill.prices_set);
/** Hourly price of a Mac of this size, from the price list (the largest class it meets). */
function macRate(vcpus, memMib) {
  const p = S.data.prices || {};
  const fits = (p.mac_classes || []).filter((c) => vcpus >= (c.min_vcpus || 0) && memMib >= (c.min_memory_gib || 0) * 1024);
  fits.sort((a, b) => (b.min_vcpus - a.min_vcpus) || (b.min_memory_gib - a.min_memory_gib));
  return fits.length ? { rate: fits[0].per_hour, cls: fits[0].name } : { rate: p.mac_per_hour || 0, cls: null };
}
/** The lowest hourly price of a Mac with at least this size. */
function macRateFrom(minVcpus, minMemMib) {
  const p = S.data.prices || {};
  const rates = (p.mac_classes || []).filter((c) => (c.min_vcpus || 0) >= minVcpus || (c.min_memory_gib || 0) * 1024 >= minMemMib).map((c) => c.per_hour);
  if (!rates.length) rates.push(...(p.mac_classes || []).map((c) => c.per_hour), p.mac_per_hour || 0);
  return Math.min(...rates.filter((r) => r > 0), Infinity);
}
function vmRate(vcpus, memMib, macos) {
  const p = S.data.prices || {};
  return (p.vm_per_hour || 0) + (p.vcpu_per_hour || 0) * vcpus + (p.memory_gib_per_hour || 0) * memMib / 1024 + (macos ? p.macos_vm_per_hour || 0 : 0);
}
const myStatement = () => (S.data.bill && (S.data.bill.statements || [])[0]) || null;

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
  const headers = { 'X-FluxVM-Console': '1' };
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
  tenantKeys: {}, // operator: tenant id -> its API keys, loaded when its details open
  logs: {}, // vm key -> the last console log text shown
  billMonth: null,
  billView: null,
  hooks: null, // webhooks, loaded on the Notifications page
  billLoading: null,
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

/** A disk path shortened to its last two parts, for VMs that were not made from a catalog image. */
function diskName(p) {
  const parts = String(p || '').split('/').filter(Boolean);
  return parts.length > 1 ? parts.slice(-2).join('/') : (parts[0] || 'Custom disk');
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
  const tunnelsP = api('GET', '/fleet/tunnels').catch(() => ({ items: [] })); // ---- tunnel ----
  const [leases, vms, overlays, images] = await Promise.all([
    api('GET', '/fleet/leases'),
    api('GET', '/fleet/vms'),
    soft(api('GET', '/fleet/overlays'), { items: [] }),
    soft(api('GET', '/fleet/images'), { items: [] }),
  ]);
  const [events, prices, bill, ntfs] = await Promise.all([
    soft(api('GET', '/fleet/events?limit=300'), { items: [] }),
    soft(api('GET', '/fleet/prices'), null),
    soft(api('GET', '/fleet/billing'), null),
    soft(api('GET', '/fleet/notifications?limit=50'), { items: [] }),
  ]);
  const d = {
    leases: (leases.items || []).sort((a, b) => (a.state === b.state ? b.started_at.localeCompare(a.started_at)
      : a.state === 'active' ? -1 : 1)),
    vms: (vms.items || []).sort((a, b) => String(a.name).localeCompare(String(b.name))),
    unreachable: vms.unreachable_nodes || [],
    overlays: overlays.items || [],
    events: events.items || [],
    notifications: ntfs.items || [],
    prices,
    bill,
    images: (images.items || []).sort((a, b) => (a.guest_os === b.guest_os ? a.name.localeCompare(b.name) : a.guest_os === 'macos' ? -1 : 1)),
    leasedNodes: {},
    drains: {},
  };
  { const t = await tunnelsP; d.tunnels = t.items || []; d.tunnelPolicy = t; } // ---- tunnel ----
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
    const [views, keys] = await Promise.all([
      Promise.all(active.map((l) => soft(api('GET', '/fleet/nodes/' + enc(l.node)), { name: l.node }))),
      soft(api('GET', '/fleet/tenants/me/keys'), { items: [] }),
    ]);
    d.keys = keys.items || [];
    active.forEach((l, i) => { d.leasedNodes[l.node] = views[i]; });
  }
  d.metricsSummary = await soft(api('GET', '/fleet/metrics/summary'), null);
  d.volumes = (await soft(api('GET', '/fleet/volumes'), { items: [] })).items || [];
  d.pools = await loadPools(); // ---- warmpool ----
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
let again = false; // a refresh was asked for while one was loading (after an action, or a new sign-in)
async function refresh({ force = false } = {}) {
  clearTimeout(timer);
  if (loading) { again = true; return; }
  loading = true;
  let signedOut = false;
  try {
    await haStatus();
    await load();
    S.error = null;
    S.loadedAt = Date.now();
    showApp();
  } catch (e) {
    if (e.status === 401) {
      showSignin(token() ? 'That token was not accepted. Check it and try again.' : S.user ? 'Your session has ended. Sign in again.' : '');
      signedOut = true;
    } else if (e.status === 403 && !S.data) {
      showSignin('This tenant is disabled. Ask your operator to enable it.');
      signedOut = true;
    } else {
      S.error = e;
      if (S.data) showApp(); else showSignin(e.message);
    }
  } finally {
    loading = false;
  }
  if (again) { again = false; return refresh({ force: true }); }
  if (signedOut) return;
  render(force);
  if (document.visibilityState === 'visible') timer = setTimeout(refresh, 10000);
}
document.addEventListener('visibilitychange', () => { if (document.visibilityState === 'visible') refresh(); });

function showSignin(msg) {
  $('app').hidden = true;
  $('signin').hidden = false;
  $('signin-error').textContent = msg || '';
  $('signin-token').focus();
  signupOffer();
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
  { id: 'overview', label: 'Overview', group: 'overview', icon: 'home', blurb: 'What is running and what needs you' },
  { id: 'macs', label: isOp() ? 'Macs' : 'My Macs', count: () => macs().length, group: 'cloud', icon: 'mac',
    blurb: isOp() ? 'Every Mac in the fleet' : 'The Macs you lease' },
  { id: 'networks', label: 'Networks', count: () => S.data.overlays.length, group: 'cloud', icon: 'network', blurb: 'Private networks across Macs' },
  { id: 'images', label: 'Images', count: () => S.data.images.length, group: 'cloud', icon: 'image', blurb: 'What you can launch' },
  { id: 'vms', label: 'Virtual machines', count: () => S.data.vms.length, group: 'compute', icon: 'vm', blurb: 'Launch, control and watch VMs' },
  { id: 'volumes', label: 'Volumes', count: () => (S.data.volumes || []).length, group: 'compute', icon: 'disk', blurb: 'Disks that outlive a VM' },
  { id: 'pools', label: 'Warm pools', count: () => (S.data.pools || []).length, group: 'compute', icon: 'zap', blurb: 'VMs booted ahead of time' }, // ---- warmpool ----
  { id: 'tunnels', label: 'Tunnels', count: () => (S.data.tunnels || []).length, group: 'compute', icon: 'term', blurb: 'SSH and port tunnels' }, // ---- tunnel ----
  { id: 'billing', label: 'Billing', group: 'billing', icon: 'card', blurb: 'Statements, budget and payment' },
  { id: 'usage', label: 'Usage', group: 'billing', icon: 'chart', blurb: 'Metered use and graphs' },
  ...(isOp() ? [{ id: 'tenants', label: 'Tenants', count: () => (S.data.tenants || []).length, group: 'access', icon: 'building', blurb: 'Accounts, quotas and leases' }] : []),
  ...(isOp() ? [] : [{ id: 'members', label: 'Members', group: 'access', icon: 'users', blurb: 'People and their roles' }]),
  ...(isOp() || !can('admin') ? [] : [{ id: 'keys', label: 'API keys', count: () => (S.data.keys || []).length, group: 'access', icon: 'key', blurb: 'Keys for CI and tools' }]),
  { id: 'activity', label: 'Activity', group: 'access', icon: 'pulse', blurb: 'Who changed what' },
  { id: 'notifications', label: 'Notifications', group: 'access', icon: 'bell', blurb: 'Alerts and webhooks' },
  ...(isOp() ? [{ id: 'policy', label: 'Policy', group: 'access', icon: 'shield', blurb: 'Rules for the whole cloud' }] : []),
];

const NAV_GROUPS = [
  { id: 'overview', label: 'Overview' },
  { id: 'cloud', label: 'Cloud' },
  { id: 'compute', label: 'Compute' },
  { id: 'billing', label: 'Billing' },
  { id: 'access', label: 'Access' },
];

// The top bar's grouped menus. Rebuilt only when what it shows changes, so an open menu is not closed by a refresh.
let navOpen = null;
let navTimer = null;
function closeNav(focusBtn) {
  const was = navOpen;
  navOpen = null;
  document.querySelectorAll('#nav .mega').forEach((m) => { m.hidden = true; });
  document.querySelectorAll('#nav .navbtn[aria-haspopup]').forEach((b) => b.setAttribute('aria-expanded', 'false'));
  if (focusBtn && was) { const b = document.getElementById('navbtn-' + was); if (b) b.focus(); }
}
function openNav(id, focusFirst) {
  closeNav();
  const btn = document.getElementById('navbtn-' + id);
  const panel = document.getElementById('mega-' + id);
  if (!btn || !panel) return;
  navOpen = id;
  panel.hidden = false;
  btn.setAttribute('aria-expanded', 'true');
  if (focusFirst) { const a = panel.querySelector('a'); if (a) a.focus(); }
}
function buildNav(pages, current) {
  const li = [];
  for (const g of NAV_GROUPS) {
    const items = pages.filter((p) => p.group === g.id);
    if (!items.length) continue;
    const active = items.some((p) => p.id === current.id);
    if (items.length === 1 && g.id === 'overview') {
      li.push(h('li', {}, h('a', { class: 'navbtn', href: '#/' + items[0].id, 'aria-current': active ? 'page' : null }, g.label)));
      continue;
    }
    const btn = h('button', { class: 'navbtn', id: 'navbtn-' + g.id, type: 'button', 'aria-haspopup': 'true',
      'aria-expanded': 'false', 'aria-controls': 'mega-' + g.id, 'aria-current': active ? 'page' : null },
    g.label, icon('chev', 12));
    btn.querySelector('svg').classList.add('chev');
    const panel = h('div', { class: 'mega', id: 'mega-' + g.id, hidden: true, role: 'region', 'aria-label': g.label },
      items.map((p) => h('a', { class: 'mega-item', href: '#/' + p.id, 'aria-current': p.id === current.id ? 'page' : null },
        icon(p.icon, 18),
        h('span', {}, h('span', { class: 'mi-title' }, p.label, p.count ? h('span', { class: 'count' }, p.count()) : null),
          h('span', { class: 'mi-blurb' }, p.blurb)))));
    const wrap = h('li', {}, btn, panel);
    btn.addEventListener('click', (e) => { e.stopPropagation(); navOpen === g.id ? closeNav() : openNav(g.id); });
    btn.addEventListener('keydown', (e) => {
      if (e.key === 'ArrowDown') { e.preventDefault(); openNav(g.id, true); }
      if (e.key === 'ArrowRight' || e.key === 'ArrowLeft') {
        const btns = [...document.querySelectorAll('#nav .navbtn')];
        const n = btns[(btns.indexOf(btn) + (e.key === 'ArrowRight' ? 1 : btns.length - 1)) % btns.length];
        if (n) { n.focus(); if (navOpen && n.id.startsWith('navbtn-')) openNav(n.id.slice(7)); }
      }
    });
    // Hover opens after a short delay and closes a little later, so moving from the button to the panel is forgiving.
    wrap.addEventListener('mouseenter', () => {
      if (!window.matchMedia('(hover: hover) and (min-width: 861px)').matches) return;
      clearTimeout(navTimer);
      navTimer = setTimeout(() => openNav(g.id), 120);
    });
    wrap.addEventListener('mouseleave', () => {
      if (!window.matchMedia('(hover: hover) and (min-width: 861px)').matches) return;
      clearTimeout(navTimer);
      navTimer = setTimeout(() => { if (navOpen === g.id) closeNav(); }, 450);
    });
    panel.addEventListener('keydown', (e) => {
      const links = [...panel.querySelectorAll('a')];
      const at = links.indexOf(document.activeElement);
      if (e.key === 'ArrowDown' || e.key === 'ArrowRight') { e.preventDefault(); links[(at + 1) % links.length].focus(); }
      if (e.key === 'ArrowUp' || e.key === 'ArrowLeft') { e.preventDefault(); links[(at + links.length - 1) % links.length].focus(); }
      if (e.key === 'Escape') { e.stopPropagation(); closeNav(true); }
    });
    panel.addEventListener('click', (e) => { if (e.target.closest('a')) { closeNav(); document.body.classList.remove('nav-open'); } });
    li.push(wrap);
  }
  return li;
}
document.addEventListener('click', (e) => { if (navOpen && !e.target.closest('#nav')) closeNav(); });
document.addEventListener('keydown', (e) => { if (e.key === 'Escape' && navOpen) closeNav(true); });

// ---- render ---------------------------------------------------------------------------------------------------------

function render(force) {
  if (!S.data) return;
  const r = route();
  const pages = PAGES();
  const page = pages.find((p) => p.id === r.page) || pages[0];

  const navKey = JSON.stringify([pages.map((p) => [p.id, p.label, p.count ? p.count() : null]), page.id, isOp()]);
  if (navKey !== S.lastNavKey) {
    S.lastNavKey = navKey;
    const reopen = navOpen;
    $('nav').replaceChildren(...buildNav(pages, page));
    if (reopen) openNav(reopen);
  }
  $('who').textContent = isOp() ? 'Operator' : (S.me.display_name || S.me.id);
  $('role').textContent = isOp() ? 'Fleet token' : 'Tenant ' + S.me.id;
  $('acct').textContent = ((isOp() ? 'O' : (S.me.display_name || S.me.id || '?')).trim()[0] || '?').toUpperCase();
  $('bar-title').textContent = page.label;
  document.title = page.label + ' · FluxVM Cloud';
  $('signout').hidden = !token() && !S.user;
  renderWho();
  renderBanners();
  renderBell();
  tick();

  const viewKey = JSON.stringify([page.id, r.id, S.data, S.me, S.usage, S.usageDays, S.policy, S.vmFilter, S.query, S.billMonth, S.billView, S.hooks]);
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
  if (S.user && S.user.role === 'viewer') out.push(h('div', { class: 'banner info', role: 'status' }, h('span', {}, roleNote())));
  if (S.error) {
    out.push(h('div', { class: 'banner bad', role: 'alert' },
      h('span', { class: 'grow' }, S.error.message + (S.loadedAt ? ' Showing what was loaded ' + rel(S.loadedAt) + '.' : '')),
      h('button', { onclick: () => refresh({ force: true }) }, 'Try again')));
  }
  out.push(...signupBanners());
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
  return h('div', { class: 'panel empty' }, h('div', { class: 'mark', 'aria-hidden': 'true' }, '+'),
    h('h3', {}, title), h('p', {}, text),
    actions.length ? h('div', { class: 'row', style: { justifyContent: 'center' } }, actions) : null);
}

const status = (s) => h('span', { class: 'status ' + s.cls }, s.text);

/** `good`: more is better (healthy Macs), so a full bar is never drawn as a warning. */
function meter(label, used, max, fmt = String, good = false) {
  const has = max !== undefined && max !== null && max > 0;
  const p = has ? Math.min(1, used / max) : 0;
  return h('div', { class: 'panel meter' },
    h('div', { class: 'muted small' }, label),
    h('div', { class: 'value' }, fmt(used), has ? h('small', {}, ' of ' + fmt(max)) : h('small', {}, ' (no limit)')),
    has ? h('div', { class: 'track' + (good ? (p >= 1 ? '' : p >= 0.5 ? ' warn' : ' bad') : (p >= 1 ? ' bad' : p >= 0.8 ? ' warn' : '')) },
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
  const left = Math.max(0, new Date(l.min_end).getTime() - Date.now());
  const hrs = Math.ceil(left / 3600000);
  return h('span', { class: 'ring' + (releasable ? ' done' : '') + (big ? ' big' : ''), style: { '--p': p.toFixed(3) },
    role: 'img', 'aria-label': releasable ? 'Minimum lease period over' : hrs + ' hours of the minimum lease period left' },
  h('span', { 'aria-hidden': 'true' }, releasable ? '✓' : hrs + 'h'));
}

function leaseLine(l) {
  if (!l) return '';
  const { releasable } = leaseProgress(l);
  return releasable ? 'Can be released now' : 'Can be released ' + rel(l.min_end);
}

/** vCPU and memory in use on a Mac, from what it reports; nothing when it has not reported a size. */
function capacity(n) {
  if (!n.vcpus_total || n.free_vcpus === undefined || !n.memory_mib_total || n.free_memory_mib === undefined) return null;
  const row = (label, used, total, text) => {
    const p = Math.min(1, Math.max(0, used / total));
    return h('div', { class: 'cap-row' }, h('span', {}, label),
      h('div', { class: 'track' + (p >= 0.9 ? ' bad' : p >= 0.7 ? ' warn' : ''), role: 'img',
        'aria-label': label + ' ' + Math.round(p * 100) + '% in use' }, h('i', { style: { width: (p * 100).toFixed(1) + '%' } })),
      h('span', { class: 'num' }, text));
  };
  return h('div', { class: 'cap' },
    row('vCPU', n.vcpus_total - n.free_vcpus, n.vcpus_total, (n.vcpus_total - n.free_vcpus) + ' / ' + n.vcpus_total),
    row('Memory', n.memory_mib_total - n.free_memory_mib, n.memory_mib_total,
      gib(n.memory_mib_total - n.free_memory_mib) + ' / ' + gib(n.memory_mib_total)));
}

function macTile(m) {
  const st = macState(m);
  const n = m.node;
  const sel = route().page === 'macs' && route().id === m.name;
  return h('button', { class: 'mac' + (sel ? ' selected' : ''), type: 'button', onclick: () => go('macs', m.name) },
    h('div', { class: 'mac-top' }, h('b', { class: 'grow' }, m.name), status(st)),
    m.isMac ? chassis(m) : h('div', { class: 'chassis' }, h('span', { class: 'muted small' }, 'Linux host'),
      h('span', { class: 'led' + (n.healthy !== false ? ' on' : '') })),
    capacity(n),
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
          + 'Leases last at least 24 hours, as Apple\'s macOS licence requires.', need('admin', h('button', { class: 'primary', onclick: leaseSheet }, 'Lease a Mac'))),
      list.length ? h('div', { class: 'macs' }, list.map(macTile))
        : empty('You have no Macs yet', 'Lease a Mac to run macOS VMs. It is yours alone until you release it, '
          + 'and you can release it once the 24-hour minimum is over.', need('admin', h('button', { class: 'primary', onclick: leaseSheet }, 'Lease a Mac')))];
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
      : 'Your VMs, on the Macs you lease and on shared hosts.', need('change', h('button', { class: 'primary', onclick: () => launchSheet() }, 'Launch VM')))];
    if (S.data.unreachable.length) {
      out.push(h('div', { class: 'note warn', style: { marginBottom: '16px' } },
        'Not listed because they did not answer: ' + S.data.unreachable.map((u) => u.node).join(', ') + '.'));
    }
    if (!all.length) {
      out.push(empty('No virtual machines yet', isOp() ? 'VMs appear here as tenants launch them.'
        : (S.data.leases.some((l) => l.state === 'active') ? 'Launch a VM from an image in the catalog.'
          : 'macOS VMs run on Macs you lease. Lease one, then launch a VM on it.'),
      !isOp() && !S.data.leases.some((l) => l.state === 'active') ? need('admin', h('button', { onclick: leaseSheet }, 'Lease a Mac')) : null,
      need('change', h('button', { class: 'primary', onclick: () => launchSheet() }, 'Launch VM'))));
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
        h('td', {}, status(vmStatus(v)), warmTag(v)),
        h('td', { class: 'hide-sm' }, img ? img.display_name || img.name : h('span', { class: 'muted', title: (v.request && v.request.image) || '' }, diskName(v.request && v.request.image))),
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
      + 'switch. Traffic between Macs is encrypted.', need('change', h('button', { class: 'primary', onclick: networkSheet }, 'Create network')))];
    if (!list.length) {
      out.push(empty('No private networks', 'Create one, then pick it when you launch a VM. VMs on it reach each '
        + 'other by IP on any of its Macs.', need('change', h('button', { class: 'primary', onclick: networkSheet }, 'Create network'))));
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
      h('td', {}, Object.keys(n.members || {}).length
        ? h('div', { class: 'chips' }, Object.keys(n.members).map((m) => h('span', { class: 'tag' }, m)))
        : h('span', { class: 'muted' }, 'none yet')),
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
    // Under an hour two decimals, so a short run does not read as 0.0.
    const hrs = (s) => (s > 0 && s < 3600 ? (s / 3600).toFixed(2) : (s / 3600).toFixed(1));
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
      if (t.vm_seconds > 0) {
        const mac = Math.min(t.macos_vm_seconds || 0, t.vm_seconds);
        const pct = (100 * mac / t.vm_seconds).toFixed(1);
        out.push(h('div', { class: 'panel pad section' }, h('h3', {}, 'Where your VM hours went'),
          h('div', { class: 'split', role: 'img', 'aria-label': hrs(mac) + ' macOS VM hours of ' + hrs(t.vm_seconds) },
            h('i', { class: 'a', style: { width: pct + '%' } }), h('i', { class: 'b', style: { width: (100 - pct) + '%' } })),
          h('div', { class: 'legend' }, h('span', {}, 'macOS ' + hrs(mac) + ' h'),
            h('span', { class: 'b' }, 'Linux ' + hrs(t.vm_seconds - mac) + ' h'))));
      } else if (!t.lease_seconds) {
        out.push(h('p', { class: 'note' }, 'Nothing recorded in the last ' + S.usageDays + ' days. Hours appear here once a VM '
          + 'runs or a Mac is leased.'));
      }
      if ((S.data.leases || []).some((l) => l.state === 'active')) {
        out.push(h('p', { class: 'note' }, 'A lease in progress is counted as it runs. Its 24-hour minimum is added to '
          + 'Billable Mac hours once you release it.'));
      }
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

  billing() {
    const cur = new Date().toISOString().slice(0, 7);
    const month = S.billMonth || cur;
    const view = S.billView && S.billView.month === month ? S.billView : null;
    // This month comes with every refresh; another month (or this one, when that failed) is fetched once.
    const data = (month === cur && S.data.bill) || (view && view.data);
    if (!data) {
      if (view && view.error) {
        return [pageHead('Billing'), h('p', { class: 'note warn' }, 'Could not load billing: ' + view.error + ' ',
          h('button', { onclick: () => { S.billView = null; render(); } }, 'Try again'))];
      }
      if (S.billLoading !== month) loadBill(month);
      return [pageHead('Billing'), h('p', { class: 'muted' }, 'Loading…')];
    }
    const b = { month, data };
    const months = [];
    const d = new Date();
    for (let i = 0; i < 4; i++) {
      const m = new Date(Date.UTC(d.getUTCFullYear(), d.getUTCMonth() - i, 1));
      months.push(m.toISOString().slice(0, 7));
    }
    const label = (m) => new Date(m + '-01T00:00:00Z').toLocaleDateString(undefined, { month: 'long', year: 'numeric', timeZone: 'UTC' });
    const pick = h('select', { 'aria-label': 'Month', style: { width: 'auto' }, onchange: (e) => { S.billMonth = e.target.value; render(); } },
      months.map((m) => h('option', { value: m, selected: m === month }, label(m))));
    const out = [pageHead('Billing', isOp() ? 'What each tenant owes for the month, from the price list. A lease keeps the price it started with.'
      : 'What your Macs and VMs cost this month. A lease is billed for at least 24 hours.', pick,
    h('button', { onclick: downloadBill }, 'Download CSV'))];
    if (isOp()) out.push(priceListPanel());
    out.push(...billingTop());
    if (!b.data.prices_set) {
      out.push(empty('No prices yet', isOp() ? 'Set a price list to turn usage into statements.' : 'Your operator has not set prices, so nothing is charged.',
        isOp() ? h('button', { class: 'primary', onclick: priceSheet }, 'Set prices') : null));
      return out;
    }
    const stmts = b.data.statements || [];
    if (isOp()) {
      const total = stmts.reduce((a, x) => a + x.total, 0);
      out.push(h('div', { class: 'meters section' },
        h('div', { class: 'panel meter' }, h('div', { class: 'muted small' }, 'Billed in ' + label(b.month)), h('div', { class: 'value' }, money(total, b.data.currency))),
        h('div', { class: 'panel meter' }, h('div', { class: 'muted small' }, 'Tenants with charges'), h('div', { class: 'value' }, stmts.filter((x) => x.total > 0).length))));
    }
    for (const st of stmts) {
      if (isOp() && !st.lines.length) continue;
      out.push(statementPanel(st));
    }
    out.push(...billingBottom());
    if (!isOp() && can('admin')) out.push(budgetPanel(stmts[0]));
    return out;
  },

  notifications() {
    const list = S.data.notifications || [];
    const out = [pageHead('Notifications', 'Events worth knowing about, newest first. Send them to Slack, PagerDuty or your own '
      + 'service with a webhook.', need('admin', h('button', { class: 'primary', onclick: webhookSheet }, 'Add webhook')))];
    out.push(h('div', { class: 'panel' }, list.length ? h('div', { class: 'list' }, list.map(ntfRow))
      : h('div', { class: 'empty' }, h('h3', {}, 'Nothing yet'), h('p', {}, isOp() ? 'Macs that stop reporting, finished drains, leases and budgets show up here.'
        : 'Leases ending soon, budget warnings and VM changes show up here.'))));
    out.push(h('div', { class: 'section' }, h('div', { class: 'section-head' }, h('h2', { class: 'grow' }, 'Webhooks')), webhookList()));
    return out;
  },

  keys() {
    const list = S.data.keys || [];
    const out = [pageHead('API keys', 'Give each person, CI system or tool its own key, so you can see who did what and revoke '
      + 'one without breaking the others.', h('button', { class: 'primary', onclick: () => keySheet('me') }, 'Create key'))];
    out.push(h('p', { class: 'note', style: { marginBottom: '16px' } }, 'Your primary token, from your operator, signs in here and '
      + 'is the only one that can create or revoke keys. A key that leaks cannot make more keys.'));
    if (!list.length) {
      out.push(empty('No API keys yet', 'Create one for your CI system (for example the GitHub Actions runner scaler) or for '
        + 'each engineer who uses fluxctl. Read-only keys suit dashboards and support.', h('button', { class: 'primary', onclick: () => keySheet('me') }, 'Create key')));
      return out;
    }
    out.push(h('div', { class: 'panel table-wrap' }, keyTable(list, 'me')));
    return out;
  },

  activity() {
    const q = S.query.trim().toLowerCase();
    const list = (S.data.events || []).filter((e) => !q || (describe(e) + ' ' + e.actor).toLowerCase().includes(q));
    const out = [pageHead('Activity', isOp() ? 'Every change made through this central: who, what, when, and whether it worked.'
      : 'Everything your team changed: launches, deletes, leases, networks and commands.')];
    if (!(S.data.events || []).length) {
      out.push(empty('Nothing has happened yet', 'Changes appear here as soon as anyone makes one.'));
      return out;
    }
    out.push(h('div', { class: 'panel table-wrap' }, h('table', {},
      h('thead', {}, h('tr', {}, h('th', {}, 'When'), h('th', {}, 'Who'), h('th', {}, 'What'), h('th', {}, 'Result'))),
      h('tbody', {}, list.length ? activityRows(list) : h('tr', {}, h('td', { colspan: '4', class: 'muted' }, 'Nothing matches.'))))));
    out.push(h('p', { class: 'hint', style: { marginTop: '12px' } }, 'Viewing pages is not recorded. Request bodies are not '
      + 'stored, only the name of what was created.'));
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
    return [...policyView(), signupPolicyPanel()];
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
          need('admin', h('button', { class: 'primary', onclick: leaseSheet }, 'Lease a Mac'))),
        step(hasVm, 2, 'Launch a VM', 'Pick an image; it starts in seconds on your Mac.',
          need('change', h('button', { class: hasLease ? 'primary' : '', onclick: () => launchSheet() }, 'Launch VM'))),
        step(hasNet, 3, 'Connect your VMs', 'Optional: a private network across your Macs.',
          need('change', h('button', { onclick: networkSheet }, 'Create network'))))));
  }
  const st = myStatement();
  if (st && st.budget && st.budget.state !== 'ok') {
    out.push(h('div', { class: 'note ' + (st.budget.state === 'over' ? 'warn' : ''), style: { marginBottom: '16px' } },
      st.budget.state === 'over' ? 'You are over this month\'s budget of ' + money(st.budget.monthly, st.currency) + '. '
        : 'You have used ' + Math.round(st.budget.used * 100) + '% of this month\'s budget of ' + money(st.budget.monthly, st.currency) + '. ',
      h('a', { href: '#/billing' }, 'See billing')));
  }
  out.push(h('div', { class: 'section meters' },
    pricesSet() && st ? h('a', { class: 'panel meter', href: '#/billing', style: { color: 'inherit', textDecoration: 'none' } },
      h('div', { class: 'muted small' }, 'Spent this month'),
      h('div', { class: 'value' }, money(st.total, st.currency)),
      st.budget ? h('div', { class: 'track' + (st.budget.used >= 1 ? ' bad' : st.budget.used >= 0.8 ? ' warn' : '') },
        h('i', { style: { width: Math.min(100, st.budget.used * 100).toFixed(1) + '%' } })) : null,
      h('div', { class: 'small muted', style: { marginTop: '6px' } }, st.budget ? 'of ' + money(st.budget.monthly, st.currency) + ' budget'
        : st.forecast !== undefined && st.forecast !== null ? 'About ' + money(st.forecast, st.currency) + ' by month end' : '')) : null,
    meter('Macs leased', u.leased_hosts || 0, q.max_leased_hosts),
    meter('VMs', u.vms || 0, q.max_vms),
    meter('vCPUs', u.vcpus || 0, q.max_vcpus),
    meter('Memory', u.memory_mib || 0, q.max_memory_mib, gib)));
  out.push(...usageTiles());
  if (list.length) {
    out.push(h('div', { class: 'section' }, h('div', { class: 'section-head' }, h('h2', { class: 'grow' }, 'Your Macs'),
      need('admin', h('button', { class: 'plain', onclick: leaseSheet }, 'Lease another'))), h('div', { class: 'macs' }, list.map(macTile))));
  }
  if ((S.data.events || []).length) {
    out.push(h('div', { class: 'section' }, h('div', { class: 'section-head' }, h('h2', { class: 'grow' }, 'Recent activity'),
      h('a', { href: '#/activity' }, 'All activity')),
    h('div', { class: 'panel table-wrap' }, h('table', {}, h('tbody', {}, activityRows(S.data.events.slice(0, 5)))))));
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
    meter('Healthy Macs', c.nodes_healthy ?? 0, c.nodes_total, String, true),
    meter('vCPUs in use', (c.vcpus_total || 0) - (c.vcpus_free || 0), c.vcpus_total),
    meter('Memory in use', (c.memory_mib_total || 0) - (c.memory_mib_free || 0), c.memory_mib_total, gib),
    meter('Macs leased', d.leases.filter((l) => l.state === 'active').length, list.filter((m) => m.isMac).length)));
  out.push(...usageTiles());
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
      tunnels: readTunnelPolicy(f, p), // ---- tunnel ----
    };
    next.max_snapshots_per_vm = Number(f.get('max_snapshots_per_vm'));
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
  h('label', { class: 'field', style: { maxWidth: '260px' } }, h('span', {}, 'Snapshots kept per VM'),
    h('input', { name: 'max_snapshots_per_vm', type: 'number', min: '0', max: '1000', value: String(p.max_snapshots_per_vm ?? 20) }),
    h('div', { class: 'hint' }, 'A tenant\'s quota can change it. 0 turns snapshots off.')),
  tunnelPolicyFields(p), // ---- tunnel ----
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
  let vmShown = false;
  if (id) {
    if (page === 'macs') { const m = macs().find((x) => x.name === id); key = m && JSON.stringify(m); content = m ? () => macInspector(m) : null; }
    if (page === 'vms') { const v = S.data.vms.find((x) => vmKey(x) === id); key = v && JSON.stringify([v, S.data.images, tunnelsFor(v).length, (S.data.tunnelPolicy || {}).enabled]); content = v ? () => vmInspector(v) : null; vmShown = !!v; }
    if (page === 'networks') { const n = S.data.overlays.find((x) => x.name === id); key = n && JSON.stringify([n, S.data.leases]); content = n ? () => netInspector(n) : null; }
    if (page === 'volumes') { const x = (S.data.volumes || []).find((v) => v.id === id); key = x && JSON.stringify([x, S.data.vms]); content = x ? () => volumeInspector(x) : null; }
    if (page === 'tenants') { const t = (S.data.tenants || []).find((x) => x.id === id); key = t && JSON.stringify([t, S.data.leases]); content = t ? () => tenantInspector(t) : null; }
    if (!content) {
      key = 'gone:' + page + id;
      content = () => [h('div', { class: 'head' }, h('h2', { class: 'grow' }, 'Not found'), closeBtn(page)),
        h('p', { class: 'muted' }, 'It may have been deleted, or it belongs to someone else.')];
    }
  }
  if (!vmShown) stopScreen();
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
  out.push(macUsage(m));
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
    if (can('change')) acts.push(h('button', { class: 'primary', onclick: () => launchSheet({ node: m.name }) }, 'Launch VM on this Mac'));
    if (l && can('admin')) {
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
  const out = [h('div', { class: 'head' }, h('div', { class: 'grow' }, h('h2', {}, v.name), status(vmStatus(v))), closeBtn('vms')),
    powerRow(v)];
  out.push(h('section', {}, facts([
    ['Mac', h('a', { href: '#/macs/' + enc(v.node) }, v.node)],
    ['Guest', guestName(guestOf(req))],
    ['Image', img ? img.display_name || img.name : h('code', {}, req.image || '–')],
    ['Size', (req.vcpus || '–') + ' vCPUs, ' + gib(req.memory_mib)],
    nets.length ? ['Networks', nets.join(', ')] : null,
    volumesOfVm(v).length ? ['Volumes', h('div', { class: 'chips' }, volumesOfVm(v).map((x) => h('a', { class: 'tag', href: '#/volumes/' + enc(x.id) }, x.name + ', ' + volumeGib(x.size_gib))))] : null,
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
  out.push(vmUsage(v));
  out.push(screenPanel(v, running));
  out.push(connectPanel(v, running)); // ---- tunnel ----
  out.push(h('section', {}, h('h3', {}, 'Run a command'),
    h('form', { class: 'row', onsubmit: (e) => { e.preventDefault(); runCommand(v, ex); } },
      h('div', { class: 'grow' }, input), h('button', { type: 'submit', disabled: !running || !can('change') }, 'Run')),
    running ? null : h('p', { class: 'hint' }, 'The VM has to be running.'),
    out2));
  out.push(logsPanel(v));
  out.push(...snapshotSections(v).filter(Boolean));
  if (can('change')) {
    out.push(h('section', {}, h('h3', {}, 'Delete'),
      h('p', { class: 'small muted', style: { marginBottom: '10px' } }, 'Stops the VM and deletes its disk. This cannot be undone.'
        + (volumesOfVm(v).length ? ' Its volumes are detached and keep their data.' : '')),
      h('button', { class: 'danger', onclick: () => deleteVm(v) }, 'Delete VM…')));
  }
  return out;
}

// ---- billing --------------------------------------------------------------------------------------------------------

async function loadBill(month) {
  S.billLoading = month;
  try {
    const data = await api('GET', '/fleet/billing?month=' + enc(month));
    S.billView = { month, data };
  } catch (e) {
    // Kept, so the page shows the failure instead of asking again on every render.
    S.billView = { month, error: e.message };
  }
  S.billLoading = null;
  render();
}

async function downloadBill() {
  try {
    const month = S.billMonth || new Date().toISOString().slice(0, 7);
    const csv = await api('GET', '/fleet/billing?format=csv&month=' + enc(month), { text: true });
    const url = URL.createObjectURL(new Blob([csv], { type: 'text/csv' }));
    const a = h('a', { href: url, download: 'fluxvm-billing-' + month + '.csv' });
    document.body.append(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(url), 60000);
    toast('Downloaded the statement for ' + month);
  } catch (e) { toast(e.message, true); }
}

function statementPanel(st) {
  const t = (S.data.tenants || []).find((x) => x.id === st.tenant);
  return h('div', { class: 'panel section' },
    h('div', { class: 'row', style: { padding: '16px 18px 4px' } },
      h('h2', { class: 'grow' }, isOp() ? (t && t.display_name) || st.tenant : 'Statement'),
      h('span', { class: 'muted small' }, st.forecast !== undefined && st.forecast !== null ? 'About ' + money(st.forecast, st.currency) + ' by month end' : '')),
    h('div', { class: 'table-wrap' }, h('table', {},
      h('thead', {}, h('tr', {}, h('th', {}, 'Item'), h('th', { class: 'r' }, 'Quantity'), h('th', { class: 'r hide-sm' }, 'Price'), h('th', { class: 'r' }, 'Amount'))),
      h('tbody', {}, st.lines.length ? st.lines.map((l) => h('tr', {},
        h('td', {}, l.description),
        h('td', { class: 'r num' }, l.quantity.toLocaleString() + ' ' + l.unit),
        h('td', { class: 'r num hide-sm muted' }, money(l.unit_price, st.currency, true) + ' / ' + l.unit.replace(/s$/, '').replace('hour', 'h')),
        h('td', { class: 'r num' }, money(l.amount, st.currency))))
        : h('tr', {}, h('td', { colspan: '4', class: 'muted' }, 'Nothing charged in this month.')),
      h('tr', {}, h('td', { colspan: '3' }, h('b', {}, 'Total')), h('td', { class: 'r num' }, h('b', {}, money(st.total, st.currency))))))));
}

function budgetPanel(st) {
  const b = st && st.budget;
  return h('form', { class: 'panel pad section stack', onsubmit: async (e) => {
    e.preventDefault();
    const v = Number(new FormData(e.target).get('monthly') || 0);
    try {
      await api('PUT', '/fleet/tenants/me/budget', { body: { monthly: v > 0 ? v : null } });
      toast(v > 0 ? 'Budget set to ' + money(v, st && st.currency) + ' a month' : 'Budget removed');
      refresh({ force: true });
    } catch (err) { toast(err.message, true); }
  } },
  h('h2', {}, 'Monthly budget'),
  h('p', { class: 'muted small' }, 'You get a warning on the Overview at 80% and when you go over. Nothing is stopped.'),
  b ? h('div', {}, h('div', { class: 'small' }, Math.round(b.used * 100) + '% used'),
    h('div', { class: 'track' + (b.used >= 1 ? ' bad' : b.used >= 0.8 ? ' warn' : '') }, h('i', { style: { width: Math.min(100, b.used * 100).toFixed(1) + '%' } }))) : null,
  h('div', { class: 'row' }, h('label', { class: 'field grow', style: { maxWidth: '260px' } }, h('span', {}, 'Budget per month (' + ((st && st.currency) || 'USD') + ')'),
    h('input', { name: 'monthly', type: 'number', min: '0', step: '1', value: b ? String(b.monthly) : '', placeholder: 'No budget' })),
  h('button', { type: 'submit', style: { alignSelf: 'flex-end' } }, 'Save budget')));
}

function priceListPanel() {
  const p = S.data.prices || {};
  const cur = p.currency || 'USD';
  const rows = [
    ...(p.mac_classes || []).map((c) => ['Leased Mac, ' + c.name + ' (' + (c.min_vcpus || 0) + '+ vCPUs, ' + (c.min_memory_gib || 0) + '+ GiB)', c.per_hour]),
    ['Leased Mac, any other', p.mac_per_hour],
    ['VM on a shared host', p.vm_per_hour], ['vCPU on a shared host', p.vcpu_per_hour],
    ['GiB of memory on a shared host', p.memory_gib_per_hour], ['macOS guest on a shared host (extra)', p.macos_vm_per_hour],
    ['GiB of snapshot storage', p.snapshot_gib_per_hour],
    ['GiB of volume storage', p.volume_gib_per_hour],
  ].filter(([, v]) => v);
  return h('div', { class: 'panel pad' },
    h('div', { class: 'row' }, h('h2', { class: 'grow' }, 'Price list'), h('button', { onclick: priceSheet }, 'Edit prices')),
    rows.length ? h('dl', { class: 'facts', style: { marginTop: '12px' } }, rows.map(([k, v]) => [h('dt', {}, k), h('dd', { class: 'num' }, money(v, cur, true) + ' an hour')]))
      : h('p', { class: 'muted small', style: { marginTop: '8px' } }, 'Everything is free until you set prices.'),
    p.charge_vms_on_leased_macs ? h('p', { class: 'hint' }, 'VMs on leased Macs are charged too.') : null);
}

function priceSheet() {
  const p = JSON.parse(JSON.stringify(S.data.prices || {}));
  const classes = p.mac_classes && p.mac_classes.length ? p.mac_classes : [{ name: 'Mac mini', min_vcpus: 8, min_memory_gib: 16, per_hour: 0 }, { name: 'Mac Studio', min_vcpus: 20, min_memory_gib: 64, per_hour: 0 }];
  const list = h('div', { class: 'stack' });
  const classRow = (c) => {
    const row = h('div', { class: 'row', 'data-class': '1' },
      h('input', { name: 'c_name', value: c.name || '', placeholder: 'Class name', 'aria-label': 'Class name', style: { flex: '2 1 120px' } }),
      h('input', { name: 'c_vcpus', type: 'number', min: '0', value: String(c.min_vcpus || 0), 'aria-label': 'Minimum vCPUs', title: 'Minimum vCPUs', style: { flex: '1 1 60px' } }),
      h('input', { name: 'c_mem', type: 'number', min: '0', value: String(c.min_memory_gib || 0), 'aria-label': 'Minimum memory GiB', title: 'Minimum memory (GiB)', style: { flex: '1 1 60px' } }),
      h('input', { name: 'c_price', type: 'number', min: '0', step: '0.01', value: String(c.per_hour || 0), 'aria-label': 'Price per hour', title: 'Price per hour', style: { flex: '1 1 80px' } }),
      h('button', { type: 'button', class: 'icon', 'aria-label': 'Remove class', onclick: () => row.remove() }, 'Remove'));
    return row;
  };
  list.append(h('div', { class: 'row small muted', 'aria-hidden': 'true' },
    h('span', { style: { flex: '2 1 120px' } }, 'Class'), h('span', { style: { flex: '1 1 60px' } }, 'Min vCPUs'),
    h('span', { style: { flex: '1 1 60px' } }, 'Min GiB'), h('span', { style: { flex: '1 1 80px' } }, 'Per hour'),
    h('span', { style: { width: '62px' } })));
  classes.forEach((c) => list.append(classRow(c)));
  const num = (name, label, value, hint) => h('label', { class: 'field' }, h('span', {}, label),
    h('input', { name, type: 'number', min: '0', step: '0.001', value: String(value || 0) }), hint ? h('div', { class: 'hint' }, hint) : null);
  openSheet({
    title: 'Price list', intro: 'Prices per hour. New leases take the price of their Mac\'s class when they start; running leases keep theirs.',
    wide: true,
    body: [
      h('label', { class: 'field', style: { maxWidth: '160px' } }, h('span', {}, 'Currency'), h('input', { name: 'currency', value: p.currency || 'USD', pattern: '[A-Z]{3}', maxlength: '3' })),
      h('div', {}, h('span', { class: 'field-label' }, 'Leased Macs by class'),
        h('p', { class: 'hint', style: { margin: '0 0 8px' } }, 'A Mac takes the largest class it meets.'),
        list, h('button', { type: 'button', class: 'plain', onclick: () => list.append(classRow({})) }, 'Add class')),
      num('mac_per_hour', 'A leased Mac in no class', p.mac_per_hour),
      h('div', { class: 'grid2' }, num('vm_per_hour', 'Each VM on a shared host', p.vm_per_hour), num('vcpu_per_hour', 'Each vCPU', p.vcpu_per_hour),
        num('memory_gib_per_hour', 'Each GiB of memory', p.memory_gib_per_hour), num('macos_vm_per_hour', 'Extra for a macOS guest', p.macos_vm_per_hour)),
      num('volume_gib_per_hour', 'Each GiB of volume storage', p.volume_gib_per_hour, 'Charged for as long as a volume exists, attached or not.'),
      check('charge_vms_on_leased_macs', !!p.charge_vms_on_leased_macs, 'Also charge VMs on leased Macs', 'Normally the lease covers everything that runs on the Mac.'),
      num('snapshot_gib_per_hour', 'Each GiB of snapshot storage', p.snapshot_gib_per_hour, 'Charged for every snapshot, on leased Macs too, for as long as it is kept.'),
    ],
    submit: 'Save prices',
    onSubmit: async (f, form) => {
      const body = {
        currency: String(f.get('currency') || 'USD').toUpperCase(),
        mac_classes: [...form.querySelectorAll('[data-class]')].map((r) => ({
          name: r.querySelector('[name=c_name]').value.trim(),
          min_vcpus: Number(r.querySelector('[name=c_vcpus]').value || 0),
          min_memory_gib: Number(r.querySelector('[name=c_mem]').value || 0),
          per_hour: Number(r.querySelector('[name=c_price]').value || 0),
        })).filter((c) => c.name),
        charge_vms_on_leased_macs: f.has('charge_vms_on_leased_macs'),
      };
      for (const k of ['mac_per_hour', 'vm_per_hour', 'vcpu_per_hour', 'memory_gib_per_hour', 'macos_vm_per_hour', 'volume_gib_per_hour']) body[k] = Number(f.get(k) || 0);
      body.snapshot_gib_per_hour = Number(f.get('snapshot_gib_per_hour') || 0);
      await api('PUT', '/fleet/prices', { body });
      toast('Prices saved');
      S.billView = null;
      await refresh({ force: true });
    },
  });
}

// ---- notifications --------------------------------------------------------------------------------------------------

const SEEN_KEY = 'fluxvm.console.seen';
const tone = (kind) => (/over|down|ended|failed|suspended/.test(kind) ? 'bad' : /near|ending|scrub/.test(kind) ? 'warn' : '');
const seenKey = () => SEEN_KEY + '.' + (isOp() ? 'op' : (S.me && S.me.id) || 'tenant');
function seenAt() { return Number(storage.get('localStorage', seenKey()) || 0); }
// The newest notification's own (server) time, not this browser's clock, so a skewed clock neither hides nor repeats any.
function markSeen() {
  const newest = Math.max(seenAt(), ...((S.data && S.data.notifications) || []).map((n) => new Date(n.time).getTime() || 0));
  storage.set('localStorage', seenKey(), String(newest));
  renderBell();
}

function ntfRow(n) {
  const fresh = new Date(n.time).getTime() > seenAt();
  return h(n.link ? 'a' : 'div', { href: n.link || null, style: { color: 'inherit', textDecoration: 'none' } },
    h('span', { class: 'ntf-dot ' + (tone(n.type) || (fresh ? 'new' : '')), 'aria-hidden': 'true' }),
    h('div', { class: 'grow' }, h('div', { style: { fontWeight: fresh ? 600 : 400 } }, n.title),
      n.detail ? h('div', { class: 'small muted' }, n.detail) : null,
      h('div', { class: 'small muted' }, rel(n.time) + (isOp() && n.tenant ? ', ' + n.tenant : ''))));
}

function renderBell() {
  if (!S.data) return;
  const list = S.data.notifications || [];
  const seen = seenAt();
  const unread = list.filter((n) => new Date(n.time).getTime() > seen).length;
  $('bell-count').hidden = !unread;
  $('bell-count').textContent = unread > 9 ? '9+' : String(unread);
  $('bell').setAttribute('aria-label', unread ? unread + ' new notifications' : 'Notifications');
  const panel = $('bell-panel');
  if (panel.hidden) return;
  panel.replaceChildren(
    h('div', { class: 'row', style: { padding: '14px 16px 6px' } }, h('h3', { class: 'grow' }, 'Notifications'),
      h('a', { href: '#/notifications', class: 'small', onclick: () => toggleBell(false) }, 'All and webhooks')),
    list.length ? h('div', { class: 'list' }, list.slice(0, 8).map(ntfRow))
      : h('p', { class: 'muted small', style: { padding: '8px 16px 16px' } }, 'Nothing yet.'));
}

function toggleBell(open) {
  const panel = $('bell-panel');
  const show = open === undefined ? panel.hidden : open;
  panel.hidden = !show;
  $('bell').setAttribute('aria-expanded', String(show));
  clearTimeout(S.seenTimer);
  if (show) { renderBell(); S.seenTimer = setTimeout(markSeen, 1500); }
}

function webhookList() {
  if (!S.hooks) {
    if (!S.hooksLoading) {
      S.hooksLoading = api('GET', '/fleet/webhooks').then((r) => { S.hooks = r; })
        .catch((e) => { S.hooks = { items: [], kinds: [] }; toast(e.message, true); })
        .finally(() => { S.hooksLoading = null; S.lastViewKey = ''; render(); });
    }
    return h('p', { class: 'muted' }, 'Loading…');
  }
  const items = S.hooks.items || [];
  if (!items.length) {
    return h('div', { class: 'panel empty' }, h('h3', {}, 'No webhooks'), h('p', {}, 'A webhook receives each event as JSON, signed with your '
      + 'secret, so a chat channel or pager hears about it right away.'), need('admin', h('button', { class: 'primary', onclick: webhookSheet }, 'Add webhook')));
  }
  return h('div', { class: 'stack' }, items.map((w) => {
    const last = (w.deliveries || [])[0];
    return h('div', { class: 'panel pad' },
      h('div', { class: 'row' }, h('div', { class: 'grow', style: { minWidth: 0 } }, h('code', { style: { overflowWrap: 'anywhere' } }, w.url),
        h('div', { class: 'small muted' }, (w.events && w.events.length ? w.events.join(', ') : 'Every event') + (isOp() && w.owner ? ', tenant ' + w.owner : ''))),
      last ? status(last.ok ? { text: 'Delivered ' + rel(last.time), cls: 'ok' } : { text: 'Failing', cls: 'bad' }) : h('span', { class: 'muted small' }, 'Nothing sent yet')),
      (w.deliveries || []).length ? h('details', { class: 'adv', style: { marginTop: '10px' } }, h('summary', {}, 'Recent deliveries'),
        h('div', { class: 'table-wrap' }, h('table', {}, h('tbody', {}, w.deliveries.map((d) => h('tr', {},
          h('td', { class: 'muted small' }, rel(d.time)), h('td', { class: 'small' }, d.type),
          h('td', {}, status(d.ok ? { text: String(d.status), cls: 'ok' } : { text: d.error || 'failed', cls: 'bad' })),
          h('td', { class: 'small muted' }, 'attempt ' + d.attempt))))))) : null,
      h('div', { class: 'row', style: { marginTop: '12px' } },
        h('button', { onclick: async (e) => {
          e.target.disabled = true;
          try {
            const r = await api('POST', '/fleet/webhooks/' + enc(w.id) + '/test');
            toast(r.delivery.ok ? 'Test delivered (' + r.delivery.status + ')' : 'Test failed: ' + (r.delivery.error || r.delivery.status), !r.delivery.ok);
          } catch (err) { toast(err.message, true); }
          S.hooks = null; render();
        } }, 'Send test'),
        h('button', { class: 'plain danger', onclick: () => confirmAct({ title: 'Delete this webhook?', body: w.url + ' stops getting events.',
          confirm: 'Delete webhook', danger: true }, 'DELETE', '/fleet/webhooks/' + enc(w.id), 'Webhook deleted', undefined, () => { S.hooks = null; }) }, 'Delete')));
  }));
}

function webhookSheet() {
  if (!allowed('admin')) return;
  const kinds = (S.hooks && S.hooks.kinds && S.hooks.kinds.length) ? S.hooks.kinds
    : ['vm.created', 'vm.deleted', 'lease.created', 'lease.released', 'lease.ending', 'lease.ended', 'budget.near', 'budget.over'];
  const label = { 'vm.created': 'VM launched', 'vm.deleted': 'VM deleted', 'lease.created': 'Mac leased', 'lease.released': 'Lease released',
    'lease.ending': 'Lease ending soon', 'lease.ended': 'Lease ended', 'budget.near': 'Budget at 80%', 'budget.over': 'Over budget',
    'node.down': 'Mac stopped reporting', 'node.scrub': 'Mac needs a scrub', 'drain.done': 'Drain finished', test: 'Test',
    'volume.created': 'Volume created', 'volume.deleted': 'Volume deleted',
    'billing.payment_failed': 'Payment failed', 'billing.suspended': 'Suspended for an unpaid invoice' };
  Object.assign(label, SNAP_KINDS);
  openSheet({
    title: 'Add a webhook', intro: 'Each event is POSTed as JSON with an X-FluxVM-Signature header (HMAC-SHA256 of the body with your secret).',
    body: [
      h('label', { class: 'field' }, h('span', {}, 'URL'), h('input', { name: 'url', type: 'url', required: true, placeholder: 'https://hooks.example.com/fluxvm', class: 'mono' }),
        h('div', { class: 'hint' }, isOp() ? 'Any URL, including internal ones.' : 'An https address on the public internet.')),
      h('div', {}, h('span', { class: 'field-label' }, 'Events'), h('p', { class: 'hint', style: { margin: '0 0 6px' } }, 'Leave all unticked to get every event.'),
        h('div', { class: 'grid2' }, kinds.filter((k) => k !== 'test').map((k) => h('label', { class: 'check' }, h('input', { type: 'checkbox', name: 'ev', value: k }),
          h('span', {}, label[k] || k))))),
    ],
    submit: 'Add webhook',
    onSubmit: async (f) => {
      const r = await api('POST', '/fleet/webhooks', { body: { url: String(f.get('url')).trim(), events: f.getAll('ev') } });
      S.hooks = null;
      refresh({ force: true });
      setTimeout(() => openSheet({ title: 'Webhook secret', intro: 'Verify each delivery with it. This is the only time it is shown.',
        body: [h('div', { class: 'token-box' }, r.secret), h('button', { type: 'button', onclick: () => copy(r.secret, 'Secret') }, 'Copy secret')],
        submit: 'Done', cancel: null, onSubmit: async () => {} }), 0);
    },
  });
}

// ---- API keys -------------------------------------------------------------------------------------------------------

function keyTable(list, owner) {
  return h('table', {},
    h('thead', {}, h('tr', {}, h('th', {}, 'Name'), h('th', {}, 'Access'), h('th', { class: 'hide-sm' }, 'Created'),
      h('th', {}, 'Last used'), h('th', { class: 'hide-sm' }, 'Expires'), h('th', {}))),
    h('tbody', {}, list.map((k) => {
      const expired = k.expires_at && new Date(k.expires_at).getTime() <= Date.now();
      return h('tr', {},
        h('td', {}, h('b', {}, k.name), h('span', { class: 'sub mono' }, k.id)),
        h('td', {}, k.read_only ? h('span', { class: 'tag' }, 'Read-only') : h('span', { class: 'tag accent' }, 'Full')),
        h('td', { class: 'hide-sm muted' }, rel(k.created_at)),
        h('td', { class: 'muted' }, k.last_used_at ? rel(k.last_used_at) : 'Never'),
        h('td', { class: 'hide-sm' }, expired ? status({ text: 'Expired', cls: 'bad' }) : k.expires_at ? rel(k.expires_at) : h('span', { class: 'muted' }, 'Never')),
        h('td', { class: 'r' }, h('button', { class: 'plain danger', onclick: () => confirmAct({
          title: 'Revoke ' + k.name + '?', body: 'Anything using this key stops working at once. Other keys are not affected.',
          confirm: 'Revoke key', danger: true }, 'DELETE', '/fleet/tenants/' + enc(owner) + '/keys/' + enc(k.id), k.name + ' revoked',
        undefined, () => { if (owner !== 'me') delete S.tenantKeys[owner]; }) }, 'Revoke')));
    })));
}

function keySheet(owner) {
  if (!allowed('admin')) return;
  openSheet({
    title: 'Create an API key', intro: 'You get the key once, on the next screen.',
    body: [
      h('label', { class: 'field' }, h('span', {}, 'Name'), h('input', { name: 'name', required: true, maxlength: '64', placeholder: 'ci-github', autocomplete: 'off' }),
        h('div', { class: 'hint' }, 'Who or what uses it. It shows in the activity log.')),
      h('label', { class: 'field' }, h('span', {}, 'Expires'), h('select', { name: 'days' },
        h('option', { value: '90' }, 'In 90 days'), h('option', { value: '30' }, 'In 30 days'), h('option', { value: '365' }, 'In a year'),
        h('option', { value: '' }, 'Never'))),
      check('read_only', false, 'Read-only', 'It can look at Macs, VMs, networks and usage, but not change anything.'),
    ],
    submit: 'Create key',
    onSubmit: async (f) => {
      const body = { name: String(f.get('name')).trim(), read_only: f.has('read_only') };
      if (f.get('days')) body.expires_in_days = Number(f.get('days'));
      const r = await api('POST', '/fleet/tenants/' + enc(owner) + '/keys', { body });
      if (owner !== 'me') delete S.tenantKeys[owner];
      refresh({ force: true });
      setTimeout(() => tokenDialog(r.key.name, r.token, true), 0);
    },
  });
}

// ---- VM power and logs ---------------------------------------------------------------------------------------------

function powerRow(v) {
  const st = vmStatus(v).text.toLowerCase();
  const path = '/fleet/vms/' + enc(v.node) + '/' + enc(v.id) + '/';
  const act = (action, label, done, confirmBody) => h('button', { onclick: async (e) => {
    if (confirmBody && !await confirmDialog({ title: label + ' ' + v.name + '?', body: confirmBody, confirm: label })) return;
    e.target.disabled = true;
    await doAct('POST', path + action, v.name + ' ' + done);
  } }, label);
  const buttons = [];
  if (st === 'stopped' || st === 'failed') buttons.push(act('start', 'Start', 'started'));
  if (st === 'running') {
    buttons.push(act('stop', 'Stop', 'stopped', 'The guest is shut down. Its disk is kept, and you can start it again.'));
    buttons.push(act('restart', 'Restart', 'restarted', 'The guest restarts. Anything not saved inside it is lost.'));
    buttons.push(act('pause', 'Pause', 'paused'));
  }
  if (st === 'paused') {
    buttons.push(act('resume', 'Resume', 'resumed'));
    buttons.push(act('stop', 'Stop', 'stopped', 'The guest is shut down. Its disk is kept.'));
  }
  return buttons.length && can('change') ? h('div', { class: 'row', style: { marginBottom: '8px' } }, buttons) : null;
}

// A console log drawn like a macOS Terminal window: traffic lights, a title, line numbers, and colour. Colour comes
// from ANSI escapes in the log itself, or, for the plain "<time> <source>: <message>" lines the daemon writes, from the
// shape of the line. Everything goes through text nodes: log text is guest-controlled and never reaches innerHTML.
const ANSI_COLORS = ['black', 'red', 'green', 'yellow', 'blue', 'magenta', 'cyan', 'white'];
const LOG_SOURCE_COLORS = ['cyan', 'magenta', 'blue', 'orange', 'green', 'yellow'];

function logSpan(cls, text) {
  return h('span', cls ? { class: cls } : {}, text);
}

// Splits `line` on SGR escapes (`ESC[31m` ...) into coloured spans; null when the line has none.
function ansiLine(line) {
  if (line.indexOf('\x1b[') < 0) return null;
  const out = [];
  let cls = [];
  let last = 0;
  const re = /\x1b\[([0-9;]*)m/g;
  let m;
  const push = (text) => { if (text) out.push(logSpan(cls.join(' '), text)); };
  while ((m = re.exec(line))) {
    push(line.slice(last, m.index));
    last = re.lastIndex;
    const codes = m[1] === '' ? [0] : m[1].split(';').map((n) => parseInt(n, 10));
    for (const c of codes) {
      if (c === 0) cls = [];
      else if (c === 1) cls = cls.filter((x) => x !== 'b').concat('b');
      else if (c === 2) cls = cls.filter((x) => x !== 'dim').concat('dim');
      else if (c === 22) cls = cls.filter((x) => x !== 'b' && x !== 'dim');
      else if (c >= 30 && c <= 37) cls = cls.filter((x) => !x.startsWith('c-')).concat('c-' + ANSI_COLORS[c - 30]);
      else if (c >= 90 && c <= 97) cls = cls.filter((x) => !x.startsWith('c-')).concat('c-' + ANSI_COLORS[c - 90] + ' bright');
      else if (c === 39) cls = cls.filter((x) => !x.startsWith('c-') && x !== 'bright');
    }
  }
  push(line.slice(last).replace(/\x1b\[[0-9;?]*[A-Za-z]/g, ''));
  return out;
}

function sourceColor(name) {
  let n = 0;
  for (let i = 0; i < name.length; i++) n = (n * 31 + name.charCodeAt(i)) >>> 0;
  return LOG_SOURCE_COLORS[n % LOG_SOURCE_COLORS.length];
}

// Colours the words that matter in a plain log line, leaving everything else untouched.
function keywordSpans(text) {
  const out = [];
  const re = /\b(error|errors|fatal|failed|failure|panic|panicked|traceback|denied|refused|timed out|oom)\b|\b(warn|warning|deprecated|retrying)\b|\b(ok|ready|running|started|connected|success|succeeded|passed|listening)\b/gi;
  let last = 0;
  let m;
  while ((m = re.exec(text))) {
    if (m.index > last) out.push(text.slice(last, m.index));
    out.push(logSpan(m[1] ? 'lv-err' : m[2] ? 'lv-warn' : 'lv-ok', m[0]));
    last = re.lastIndex;
  }
  if (last < text.length) out.push(text.slice(last));
  return out;
}

function plainLine(line) {
  const out = [];
  let rest = line;
  const ts = /^(\d{4}-\d{2}-\d{2}[T ][0-9:.]+(?:Z|[+-]\d\d:?\d\d)?)\s+/.exec(rest);
  if (ts) {
    // Just the time of day, to the millisecond: the date and offset are on hover (the panes are narrow).
    const t = /[T ](\d\d:\d\d:\d\d(?:\.\d{1,3})?)/.exec(ts[1]);
    const s = h('span', { class: 'ts', title: ts[1] }, t ? t[1] : ts[1]);
    out.push(s, ' ');
    rest = rest.slice(ts[0].length);
  }
  const src = /^([A-Za-z][\w.-]{0,31})(\[\d+\])?:\s+/.exec(rest);
  if (src) {
    out.push(logSpan('src c-' + sourceColor(src[1]), src[1] + (src[2] || '') + ':'), ' ');
    rest = rest.slice(src[0].length);
  }
  const bracket = /^\[\s*(OK|FAILED|SKIP|WARN)\s*\]\s*/.exec(rest);
  if (bracket) {
    const cls = bracket[1] === 'OK' ? 'lv-ok' : bracket[1] === 'FAILED' ? 'lv-err' : 'lv-warn';
    out.push(logSpan(cls + ' b', '[ ' + bracket[1] + ' ]'), ' ');
    rest = rest.slice(bracket[0].length);
  }
  return out.concat(keywordSpans(rest));
}

// The window: `text` is the raw log. Returns the element and a function that swaps the text in place.
function termWindow(title, text) {
  const body = h('div', { class: 'mterm-body', tabindex: '0', role: 'log', 'aria-label': title });
  const count = h('span', { class: 'mterm-meta' });
  const fill = (raw) => {
    body.textContent = '';
    const lines = String(raw).replace(/\r\n?/g, '\n').replace(/\n+$/, '').split('\n');
    for (const line of lines) body.append(h('div', { class: 'mterm-line' }, ansiLine(line) || plainLine(line)));
    body.append(h('div', { class: 'mterm-line mterm-cursor', 'aria-hidden': 'true' }, h('span', { class: 'caret' }, ' ')));
    count.textContent = lines.length + (lines.length === 1 ? ' line' : ' lines');
    body.scrollTop = body.scrollHeight;
  };
  fill(text || '');
  const el = h('div', { class: 'mterm' },
    h('div', { class: 'mterm-bar' },
      h('span', { class: 'mterm-dots', 'aria-hidden': 'true' }, h('i', { class: 'r' }), h('i', { class: 'y' }), h('i', { class: 'g' })),
      h('span', { class: 'mterm-title' }, title), count),
    body);
  return { el, fill };
}

function logsPanel(v) {
  const k = vmKey(v);
  // A console log can hold secrets (an environment dump, a typed password): the server refuses viewers, so say why.
  if (S.user && S.user.role === 'viewer') {
    return h('section', {}, h('h3', {}, 'Console log'),
      h('p', { class: 'hint' }, 'Console logs and live screens can hold secrets, so viewers do not get them. Ask an admin of your team for the member role.'));
  }
  const win = termWindow(v.name + ' \u2014 console log', S.logs[k] || '');
  win.el.hidden = !S.logs[k];
  const btn = h('button', { class: 'plain' }, S.logs[k] ? 'Refresh' : 'Show log');
  const copy = h('button', { class: 'plain', hidden: !S.logs[k] }, 'Copy');
  copy.addEventListener('click', async () => {
    try { await navigator.clipboard.writeText(S.logs[k] || ''); copy.textContent = 'Copied'; } catch (e) { copy.textContent = 'Not copied'; }
    setTimeout(() => { copy.textContent = 'Copy'; }, 1400);
  });
  btn.addEventListener('click', async () => {
    btn.disabled = true;
    try {
      const text = await api('GET', '/fleet/vms/' + enc(v.node) + '/' + enc(v.id) + '/logs?lines=300', { text: true });
      S.logs[k] = typeof text === 'string' && text.trim() ? text : 'The log is empty.';
    } catch (e) { S.logs[k] = 'The log could not be loaded: ' + e.message; }
    btn.disabled = false;
    btn.textContent = 'Refresh';
    copy.hidden = false;
    win.fill(S.logs[k]);
    win.el.hidden = false;
  });
  return h('section', {}, h('div', { class: 'row' }, h('h3', { class: 'grow' }, 'Console log'), copy, btn), win.el);
}

// ---- metrics --------------------------------------------------------------------------------------------------------
// Usage graphs from central's collector (GET /fleet/metrics/*). No chart library: the plot is an SVG stretched to its
// box (lines keep a 2px stroke), and the text, end markers, crosshair and tooltip are HTML laid over it.

const RANGES = [['1h', 3600], ['6h', 21600], ['24h', 86400]];
const RANGE_KEY = 'fluxvm.console.range';
const M = { range: storage.get('sessionStorage', RANGE_KEY) || '1h', cache: {}, live: [] };
if (!RANGES.some(([r]) => r === M.range)) M.range = '1h';
const SVGNS = 'http://www.w3.org/2000/svg';
function svgEl(tag, attrs) {
  const e = document.createElementNS(SVGNS, tag);
  for (const [k, v] of Object.entries(attrs || {})) if (v !== undefined && v !== null) e.setAttribute(k, v);
  return e;
}
const num = (x) => typeof x === 'number' && Number.isFinite(x);
const fmtPct = (x) => (num(x) ? (x > 0 && x < 10 ? x.toFixed(1) : String(Math.round(x))) + '%' : '–');
const fmtMib = (x) => (num(x) ? (x >= 1024 ? (x / 1024).toFixed(1) + ' GiB' : Math.round(x) + ' MiB') : '–');
const fmtCount = (x) => (num(x) ? String(Math.round(x)) : '–');
function fmtRate(x) {
  if (!num(x)) return '–';
  const u = [[1e9, 'GB/s'], [1e6, 'MB/s'], [1e3, 'kB/s']].find(([d]) => x >= d);
  return u ? (x / u[0]).toFixed(1) + ' ' + u[1] : Math.round(x) + ' B/s';
}
const clock = (t) => new Date(t * 1000).toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' });
const clockDay = (t) => new Date(t * 1000).toLocaleString(undefined, { weekday: 'short', hour: '2-digit', minute: '2-digit' });
const spanText = (s) => RANGES.find(([, x]) => x === s)[0].replace('h', ' hours').replace(/^1 hours/, 'hour');

/** The smallest 1, 2 or 5 × 10ⁿ at or above `v`. */
function niceMax(v) {
  if (!(v > 0)) return 1;
  const p = Math.pow(10, Math.floor(Math.log10(v)));
  return [1, 2, 5, 10].map((m) => m * p).find((m) => m >= v * 0.999);
}

/**
 * A line chart over the last `span` seconds. `series`: [{ key, label, color }]; `points`: [{ t, [key]: number|null }],
 * one a minute. `max`: a fixed top (100 for percent, a VM's memory size); otherwise a round number above the data and
 * at least `floor`, so a quiet line stays low instead of filling the box. Gaps (nothing measured, minutes missing)
 * break the line. Hover or arrow keys show every series' value at a minute.
 */
function lineChart({ title, sub, series, points, span, fmt, max, floor = 0, compact = false }) {
  const now = Date.now() / 1000;
  const step = 60;
  const t0 = now - span;
  const vals = points.flatMap((p) => series.map((s) => p[s.key])).filter(num);
  const top = max > 0 ? Math.max(max, ...vals) : niceMax(Math.max(floor, ...vals, 0));
  const X = (t) => Math.max(0, Math.min(1000, ((t + step / 2 - t0) / span) * 1000));
  const Y = (v) => 100 - (Math.max(0, Math.min(top, v)) / top) * 100;
  const plot = svgEl('svg', { viewBox: '0 0 1000 100', preserveAspectRatio: 'none', 'aria-hidden': 'true', class: 'chart-svg', focusable: 'false' });
  if (!compact) for (const g of [0, 50]) plot.append(svgEl('line', { x1: 0, x2: 1000, y1: g, y2: g, class: 'chart-grid' }));
  const last = {};
  const stats = {};
  for (const s of series) {
    const runs = [];
    let run = [];
    let prevT = null;
    const seen = [];
    for (const p of points) {
      const v = p[s.key];
      if (!num(v) || (prevT !== null && p.t - prevT > step * 2.5)) { if (run.length) runs.push(run); run = []; }
      if (num(v)) { run.push([X(p.t), Y(v)]); last[s.key] = { t: p.t, v }; seen.push(v); }
      prevT = p.t;
    }
    if (run.length) runs.push(run);
    if (seen.length) stats[s.key] = { avg: seen.reduce((a, b) => a + b, 0) / seen.length, max: Math.max(...seen) };
    for (const r of runs) {
      const d = r.map(([x, y], i) => (i ? 'L' : 'M') + x.toFixed(1) + ' ' + y.toFixed(2)).join('');
      if (series.length === 1 && r.length > 1) {
        plot.append(svgEl('path', { d: d + 'L' + r[r.length - 1][0].toFixed(1) + ' 100L' + r[0][0].toFixed(1) + ' 100Z', class: 'chart-area', style: 'fill:' + s.color }));
      }
      plot.append(svgEl('path', { d: r.length === 1 ? d + 'l0.01 0' : d, class: 'chart-line', style: 'stroke:' + s.color }));
    }
  }
  const label = title + ', last ' + spanText(span) + ': ' + series.map((s) => (series.length > 1 ? s.label + ' ' : '')
    + (stats[s.key] ? 'now ' + fmt(last[s.key].v) + ', average ' + fmt(stats[s.key].avg) + ', highest ' + fmt(stats[s.key].max) : 'not measured')).join('; ');
  const box = h('div', { class: 'chart-plot', tabindex: '0', role: 'img', 'aria-label': label });
  box.append(plot);
  for (const s of series) {
    const l = last[s.key];
    if (l) box.append(h('i', { class: 'chart-dot', style: { left: X(l.t) / 10 + '%', top: Y(l.v) + '%', '--c': s.color } }));
  }
  if (!compact) box.append(h('span', { class: 'chart-ymax', 'aria-hidden': 'true' }, fmt(top)));
  const cross = h('i', { class: 'chart-cross', hidden: true });
  const tip = h('div', { class: 'chart-tip', hidden: true, 'aria-live': 'polite' });
  box.append(cross, tip);
  let at = -1;
  const show = (i) => {
    if (i < 0 || i >= points.length) { cross.hidden = true; tip.hidden = true; at = -1; return; }
    at = i;
    const p = points[i];
    const x = X(p.t) / 10;
    cross.hidden = false;
    cross.style.left = x + '%';
    tip.replaceChildren(h('div', { class: 'chart-tip-t' }, span > 6 * 3600 ? clockDay(p.t) : clock(p.t)),
      ...series.map((s) => h('div', { class: 'chart-tip-row' }, h('b', {}, fmt(p[s.key])),
        series.length > 1 ? h('span', { class: 'key', style: { '--c': s.color } }, s.label) : null)));
    tip.hidden = false;
    tip.classList.toggle('flip', x > 55);
    tip.style.left = x + '%';
  };
  const nearest = (frac) => {
    const t = t0 + frac * span - step / 2;
    let best = -1;
    let bd = Infinity;
    points.forEach((p, i) => { const dd = Math.abs(p.t - t); if (dd < bd) { bd = dd; best = i; } });
    return best;
  };
  box.addEventListener('pointermove', (e) => { const r = box.getBoundingClientRect(); show(nearest((e.clientX - r.left) / Math.max(1, r.width))); });
  box.addEventListener('pointerleave', () => { if (document.activeElement !== box) show(-1); });
  box.addEventListener('focus', () => show(points.length - 1));
  box.addEventListener('blur', () => show(-1));
  box.addEventListener('keydown', (e) => {
    const move = { ArrowLeft: -1, ArrowRight: 1 }[e.key];
    const end = points.length - 1;
    if (move) show(Math.max(0, Math.min(end, (at < 0 ? end : at) + move)));
    else if (e.key === 'Home') show(0);
    else if (e.key === 'End') show(end);
    else return;
    e.preventDefault();
  });
  if (compact) return h('div', { class: 'chart compact' }, box);
  const head = h('div', { class: 'chart-head' }, h('span', { class: 'chart-title' }, title), sub ? h('span', { class: 'chart-sub' }, sub) : null,
    series.length > 1
      ? h('span', { class: 'chart-legend' }, series.map((s) => h('span', { class: 'key', style: { '--c': s.color } }, s.label + ' ', h('b', {}, fmt(last[s.key] && last[s.key].v)))))
      : h('span', { class: 'chart-now num' }, fmt(last[series[0].key] && last[series[0].key].v)));
  const axis = h('div', { class: 'chart-x', 'aria-hidden': 'true' }, h('span', {}, span > 6 * 3600 ? clockDay(t0) : clock(t0)),
    h('span', {}, span > 6 * 3600 ? clockDay(t0 + span / 2) : clock(t0 + span / 2)), h('span', {}, 'now'));
  return h('figure', { class: 'chart' }, head, box, axis);
}

/** A chart's place when its series has no values: what is missing and why, in the same footprint. */
function chartEmpty(title, text) {
  return h('figure', { class: 'chart' }, h('div', { class: 'chart-head' }, h('span', { class: 'chart-title' }, title),
    h('span', { class: 'chart-now muted' }, '–')), h('div', { class: 'chart-empty' }, text));
}

/**
 * The "Usage" section of an inspector: a range toggle above the charts, drawn from a cache at once and refreshed in
 * place (the old drawing stays, dimmed, while new data loads). `draw(data, span)` returns the charts.
 */
function usagePanel(kind, id, path, draw) {
  const body = h('div', { class: 'charts' });
  const keyNow = () => kind + ':' + id + ':' + M.range;
  const spanNow = () => RANGES.find(([r]) => r === M.range)[1];
  let busy = false;
  const fill = async (force) => {
    const key = keyNow();
    const c = M.cache[key];
    if (c) body.replaceChildren(...draw(c.data, spanNow()));
    else if (!body.childElementCount) body.replaceChildren(h('p', { class: 'small muted' }, 'Loading usage…'));
    if (busy || (!force && c && Date.now() - c.at < 20000)) return;
    busy = true;
    body.classList.add('stale');
    try {
      const data = await api('GET', path + '?range=' + M.range);
      M.cache[key] = { data, at: Date.now() };
      if (key === keyNow()) body.replaceChildren(...draw(data, spanNow()));
    } catch (e) {
      if (key === keyNow() && !M.cache[key]) {
        body.replaceChildren(h('p', { class: 'small muted' }, e.status === 404 || e.status === 405
          ? 'Usage graphs are not available from this central yet.' : 'Usage could not be loaded: ' + e.message));
      }
    } finally {
      busy = false;
      body.classList.remove('stale');
    }
  };
  const seg = h('div', { class: 'seg', role: 'group', 'aria-label': 'Time range' }, RANGES.map(([r]) => h('button', {
    type: 'button', 'aria-pressed': String(r === M.range), title: 'Last ' + spanText(RANGES.find(([x]) => x === r)[1]),
    onclick: () => {
      M.range = r;
      storage.set('sessionStorage', RANGE_KEY, r);
      for (const b of seg.children) b.setAttribute('aria-pressed', String(b.textContent === r));
      fill(false);
    },
  }, r)));
  const sec = h('section', { class: 'usage' }, h('div', { class: 'row', style: { marginBottom: '12px' } },
    h('h3', { class: 'grow', style: { margin: 0 } }, 'Usage'), seg), body);
  sec.refreshUsage = () => fill(true);
  M.live = M.live.filter((x) => x.isConnected);
  M.live.push(sec);
  fill(false);
  return sec;
}
setInterval(() => {
  if (document.visibilityState !== 'visible') return;
  M.live = M.live.filter((x) => x.isConnected);
  for (const s of M.live) s.refreshUsage();
}, 30000);

/** When history starts later than the range does (central keeps it in memory since it started). */
function coverage(points, span) {
  if (!points.length) return null;
  const from = points[0].t;
  return from > Date.now() / 1000 - span * 0.85
    ? h('p', { class: 'hint' }, 'Collected since ' + clock(from) + '. Central keeps up to 24 hours, in memory.') : null;
}

function vmUsage(v) {
  const req = v.request || {};
  return usagePanel('vm', vmKey(v), '/fleet/metrics/vms/' + enc(v.node) + '/' + enc(v.id), (d, span) => {
    const pts = d.points || [];
    if (!pts.length) return [h('div', { class: 'chart-empty' }, d.note || 'No samples yet.')];
    const has = (...keys) => pts.some((p) => keys.some((k) => num(p[k])));
    if (!has('cpu_percent', 'memory_used_mib', 'disk_read_bps', 'disk_write_bps', 'net_rx_bps', 'net_tx_bps')) {
      return [h('div', { class: 'chart-empty' }, d.note || 'Nothing was measured in this range.')];
    }
    const vcpus = d.vcpus || req.vcpus;
    const mem = d.memory_mib || req.memory_mib;
    const out = [
      has('cpu_percent') ? lineChart({ title: 'CPU', sub: vcpus ? 'of ' + plural(vcpus, 'vCPU') : null, span, points: pts, fmt: fmtPct, max: 100,
        series: [{ key: 'cpu_percent', label: 'CPU', color: 'var(--series-1)' }] })
        : chartEmpty('CPU', 'Not measured yet. CPU needs two samples a few seconds apart.'),
      has('memory_used_mib') ? lineChart({ title: 'Memory', sub: mem ? 'of ' + gib(mem) : null, span, points: pts, fmt: fmtMib, max: mem,
        series: [{ key: 'memory_used_mib', label: 'Memory', color: 'var(--series-1)' }] })
        : chartEmpty('Memory', 'Not measured.'),
      has('disk_read_bps', 'disk_write_bps') ? lineChart({ title: 'Disk', span, points: pts, fmt: fmtRate, floor: 1e5,
        series: [{ key: 'disk_read_bps', label: 'Read', color: 'var(--series-1)' }, { key: 'disk_write_bps', label: 'Write', color: 'var(--series-2)' }] })
        : chartEmpty('Disk', 'Not measured for this VM.'),
      has('net_rx_bps', 'net_tx_bps') ? lineChart({ title: 'Network', span, points: pts, fmt: fmtRate, floor: 1e4,
        series: [{ key: 'net_rx_bps', label: 'In', color: 'var(--series-1)' }, { key: 'net_tx_bps', label: 'Out', color: 'var(--series-2)' }] })
        : chartEmpty('Network', /network/i.test(d.note || '') ? d.note : 'Not measured for this VM.'),
    ];
    // A note about network traffic already sits in the empty network chart.
    if (d.note && !(/network/i.test(d.note) && !has('net_rx_bps', 'net_tx_bps'))) out.push(h('p', { class: 'hint' }, d.note));
    const cov = coverage(pts, span);
    if (cov) out.push(cov);
    return out;
  });
}

function macUsage(m) {
  return usagePanel('mac', m.name, '/fleet/metrics/nodes/' + enc(m.name), (d, span) => {
    const pts = d.points || [];
    if (!pts.length) return [h('div', { class: 'chart-empty' }, d.note || 'No samples yet.')];
    const has = (k) => pts.some((p) => num(p[k]));
    const memTotal = d.memory_total_mib || m.node.memory_mib_total;
    const out = [
      has('cpu_percent') ? lineChart({ title: 'CPU', sub: d.cpus ? plural(d.cpus, 'core') : null, span, points: pts, fmt: fmtPct, max: 100,
        series: [{ key: 'cpu_percent', label: 'CPU', color: 'var(--series-1)' }] })
        : chartEmpty('CPU', 'Not measured yet. CPU needs two samples a few seconds apart.'),
      has('memory_used_mib') ? lineChart({ title: 'Memory', sub: memTotal ? 'of ' + gib(memTotal) : null, span, points: pts, fmt: fmtMib, max: memTotal,
        series: [{ key: 'memory_used_mib', label: 'Memory', color: 'var(--series-1)' }] })
        : chartEmpty('Memory', 'This Mac does not report its memory.'),
      lineChart({ title: 'VMs', sub: 'on this Mac', span, points: pts, fmt: fmtCount, floor: 2,
        series: [{ key: 'vms', label: 'VMs', color: 'var(--series-1)' }] }),
    ];
    const free = d.latest && d.latest.disk_free_bytes;
    if (num(free)) out.push(h('p', { class: 'hint' }, (free / 1e9).toFixed(0) + ' GB free on the disk that holds its VMs.'));
    const cov = coverage(pts, span);
    if (cov) out.push(cov);
    return out;
  });
}

/** Overview tiles: CPU and memory in use now across the fleet (or the tenant's Macs), with the last hour. */
function usageTiles() {
  const s = S.data.metricsSummary;
  if (!s || (!isOp() && !s.nodes_reporting)) return [];
  const pts = s.points || [];
  // The trend is scaled to the data (at least 0-50%) so an hour's movement shows; the tooltip gives exact values.
  const tile = (label, pct, sub, key) => h('div', { class: 'panel meter usage-tile' },
    h('div', { class: 'usage-tile-text' }, h('div', { class: 'muted small' }, label),
      h('div', { class: 'value' }, num(pct) ? fmtPct(pct) : '–'),
      h('div', { class: 'small muted' }, sub)),
    pts.some((p) => num(p[key])) ? lineChart({ title: label, series: [{ key, label, color: 'var(--series-1)' }], points: pts, span: 3600, fmt: fmtPct, floor: 25, compact: true }) : null);
  const waiting = 'Waiting for the first sample from the Macs';
  return [h('div', { class: 'section meters' },
    tile(isOp() ? 'Fleet CPU' : 'CPU on your Macs', s.cpu_percent, s.nodes_reporting ? 'Last hour, ' + plural(Math.round(s.cpus), 'core') : waiting, 'cpu_percent'),
    tile(isOp() ? 'Fleet memory' : 'Memory on your Macs', s.memory_percent,
      s.memory_total_mib ? gib(s.memory_used_mib) + ' of ' + gib(s.memory_total_mib) + ' in use' : waiting, 'memory_percent'))];
}

// ---- the VM's screen -------------------------------------------------------------------------------------------------

// One live screen at a time: the VM open in the inspector. Its elements survive re-renders, so the picture does not
// flicker while the rest of the inspector refreshes.
const screen = { key: null, running: false, el: null, img: null, note: null, link: null, toggle: null, url: null,
  timer: null, paused: false, seq: 0 };

function setControl(on) {
  if (!screen.img) return;
  screen.control = on;
  screen.ctl.setAttribute('aria-pressed', String(on));
  screen.ctl.textContent = on ? 'Stop control' : 'Control';
  screen.bar.hidden = !on;
  screen.img.classList.toggle('controlling', on);
  screen.el.classList.toggle('screen-full', on);
  // The full window lives at the top of the page (the inspector would trap it under its own stacking order); the
  // inspector shows a stub meanwhile.
  if (on && !screen.backdrop) {
    screen.backdrop = h('div', { class: 'screen-backdrop', 'aria-hidden': 'true', onclick: () => setControl(false) });
    screen.stub = h('section', {}, h('h3', {}, 'Screen'), h('p', { class: 'hint' }, 'You are controlling this VM in the large view.'));
    if (screen.el.isConnected) screen.el.replaceWith(screen.stub);
    document.body.append(screen.backdrop, screen.el);
  } else if (!on && screen.backdrop) {
    screen.backdrop.remove();
    screen.backdrop = null;
    if (screen.stub && screen.stub.isConnected) screen.stub.replaceWith(screen.el); else screen.el.remove();
    screen.stub = null;
  }
  if (on) {
    screen.img.tabIndex = 0;
    screen.img.focus();
    if (screen.paused) { screen.paused = false; screen.toggle.textContent = 'Pause'; }
    fetchScreen(screen.key);
  } else screen.img.removeAttribute('tabindex');
}

/** Where a pointer event falls in the screenshot's own pixels (the image is letterboxed by object-fit: contain). */
function screenPoint(img, e) {
  const r = img.getBoundingClientRect();
  const nw = img.naturalWidth || 1;
  const nh = img.naturalHeight || 1;
  const scale = Math.min(r.width / nw, r.height / nh);
  const x = (e.clientX - r.left - (r.width - nw * scale) / 2) / scale;
  const y = (e.clientY - r.top - (r.height - nh * scale) / 2) / scale;
  if (x < 0 || y < 0 || x > nw || y > nh) return null;
  return { x: Math.round(x), y: Math.round(y), screen_width: nw };
}

let inputQueue = [];
let inputTimer = null;
let inputBusy = false;
function sendInput(actions) {
  inputQueue.push(...actions);
  clearTimeout(inputTimer);
  inputTimer = setTimeout(flushInput, 80);
}
async function flushInput() {
  const k = screen.key;
  if (inputBusy || !k || !inputQueue.length) return;
  inputBusy = true;
  const actions = inputQueue.splice(0, 200);
  // Consecutive type actions become one.
  const merged = [];
  for (const a of actions) {
    const last = merged[merged.length - 1];
    if (a.action === 'type' && last && last.action === 'type' && last.text.length + a.text.length <= 4096) last.text += a.text;
    else merged.push({ ...a });
  }
  const [node, id] = k.split('/');
  try {
    await api('POST', '/fleet/vms/' + enc(node) + '/' + enc(id) + '/input', { body: { actions: merged } });
  } catch (e) { toast(e.message, true); }
  inputBusy = false;
  if (screen.key !== k) { inputQueue = []; return; }
  clearTimeout(screen.timer);
  screen.timer = setTimeout(() => fetchScreen(k), 250);
  if (inputQueue.length) flushInput();
}

const NAMED = { Enter: 'return', Tab: 'tab', Escape: 'escape', Backspace: 'backspace', Delete: 'delete', ArrowUp: 'up', ArrowDown: 'down',
  ArrowLeft: 'left', ArrowRight: 'right', Home: 'home', End: 'end', PageUp: 'page_up', PageDown: 'page_down', ' ': 'space' };

// The guest types printable ASCII, newline and tab (US layout); anything else would fail the whole batch.
const typable = (t) => /^[\x20-\x7e\n\t]*$/.test(t);

/** The key a shortcut names: the physical letter or digit, so Option+E is `e` rather than the accent it would type. */
function shortcutKey(e) {
  if (/^Key[A-Z]$/.test(e.code)) return e.code.slice(3).toLowerCase();
  if (/^Digit[0-9]$/.test(e.code)) return e.code.slice(5);
  return e.key.length === 1 && typable(e.key) && e.key !== ' ' ? e.key.toLowerCase() : null;
}

function wireScreenInput(img) {
  const at = (e, action) => {
    if (!screen.control) return null;
    const p = screenPoint(img, e);
    if (p) sendInput([{ action, ...p }]);
    return p;
  };
  // A double click would otherwise reach the guest as a click and then a double click (three clicks); a single click
  // waits briefly to see whether a second one follows.
  let pendingClick = null;
  img.addEventListener('click', (e) => {
    if (!screen.control || e.detail > 1) return;
    const p = screenPoint(img, e);
    if (!p) return;
    clearTimeout(pendingClick);
    pendingClick = setTimeout(() => { pendingClick = null; if (screen.control) sendInput([{ action: 'click', ...p }]); }, 250);
  });
  img.addEventListener('dblclick', (e) => {
    clearTimeout(pendingClick);
    pendingClick = null;
    at(e, 'double_click');
  });
  img.addEventListener('contextmenu', (e) => { if (screen.control) { e.preventDefault(); at(e, 'right_click'); } });
  let wheelAt = 0;
  img.addEventListener('wheel', (e) => {
    if (!screen.control) return;
    e.preventDefault();
    if (Date.now() - wheelAt < 60) return;
    wheelAt = Date.now();
    const p = screenPoint(img, e) || {};
    const notches = (d) => Math.sign(d) * Math.min(5, Math.ceil(Math.abs(d) / 40));
    sendInput([{ action: 'scroll', dx: notches(e.deltaX), dy: notches(e.deltaY), ...p }]);
  }, { passive: false });
  img.addEventListener('keydown', (e) => {
    if (!screen.control) return;
    // Every key belongs to the VM while controlling: Escape, / and ⌘K must not reach the console's own shortcuts.
    e.stopPropagation();
    if (e.isComposing || ['Shift', 'Control', 'Alt', 'Meta', 'CapsLock', 'Dead', 'Unidentified', 'Process'].includes(e.key)) return;
    e.preventDefault();
    const modifiers = [e.metaKey && 'command', e.ctrlKey && 'control', e.altKey && 'option', e.shiftKey && 'shift'].filter(Boolean);
    const plain = !e.metaKey && !e.ctrlKey && !e.altKey;
    if (e.key.length === 1 && plain) {
      if (typable(e.key)) sendInput([{ action: 'type', text: e.key }]);
      return;
    }
    const named = NAMED[e.key] || (/^F([1-9]|1[0-2])$/.test(e.key) ? e.key.toLowerCase() : null);
    if (named) { sendInput([{ action: 'key', key: named, modifiers: plain ? modifiers.filter((m) => m === 'shift') : modifiers }]); return; }
    const key = plain ? null : shortcutKey(e);
    if (key) sendInput([{ action: 'key', key, modifiers }]);
  });
}

function stopScreen() {
  clearTimeout(screen.timer);
  screen.timer = null;
  if (screen.url) URL.revokeObjectURL(screen.url);
  screen.seq++; // a fetch still in flight is dropped
  inputQueue = [];
  if (screen.backdrop) { screen.backdrop.remove(); screen.backdrop = null; }
  if (screen.el && screen.el.parentNode === document.body) screen.el.remove();
  screen.stub = null;
  Object.assign(screen, { key: null, el: null, img: null, url: null, paused: false, control: false });
}

function screenPanel(v, running) {
  // The server refuses viewers a live screen (it can show secrets); say so instead of showing a broken picture.
  if (S.user && S.user.role === 'viewer') {
    return h('section', {}, h('h3', {}, 'Screen'),
      h('p', { class: 'hint' }, 'Live screens and console logs can hold secrets, so viewers do not get them. Ask an admin of your team for the member role.'));
  }
  const k = vmKey(v);
  const wasRunning = screen.running;
  screen.running = running;
  if (screen.key === k && running && !wasRunning) fetchScreen(k);
  if (screen.key !== k) {
    stopScreen();
    screen.key = k;
    screen.img = h('img', { class: 'screen', alt: 'The screen of ' + v.name, hidden: true });
    screen.note = h('p', { class: 'hint' }, running ? 'Connecting…' : 'The VM is not running, so it has no screen.');
    screen.link = h('a', { class: 'small', target: '_blank', rel: 'noopener', hidden: true }, 'Open full size');
    screen.toggle = h('button', { class: 'plain', type: 'button', onclick: () => {
      screen.paused = !screen.paused;
      screen.toggle.textContent = screen.paused ? 'Resume' : 'Pause';
      if (!screen.paused) fetchScreen(screen.key);
    } }, 'Pause');
    screen.control = false;
    screen.ctl = h('button', { class: 'plain', type: 'button', 'aria-pressed': 'false', title: 'Send your clicks and typing to the VM', hidden: !can('change'),
      onclick: () => setControl(!screen.control) }, 'Control');
    screen.typeIn = h('input', { placeholder: 'Type or paste text…', 'aria-label': 'Text to type into the VM', class: 'grow', style: { width: 'auto' } });
    const keyBtn = (label, key, modifiers) => h('button', { type: 'button', class: 'icon', onclick: () => sendInput([{ action: 'key', key, modifiers: modifiers || [] }]) }, label);
    screen.bar = h('form', { class: 'row', hidden: true, style: { marginTop: '8px' }, onsubmit: (e) => {
      e.preventDefault();
      const text = screen.typeIn.value.replace(/\r\n?/g, '\n');
      if (!text) return;
      if (!typable(text)) { toast('Only plain ASCII text, newlines and tabs can be typed into the VM (US keyboard).', true); return; }
      sendInput([{ action: 'type', text: text.slice(0, 4096) }]);
      screen.typeIn.value = '';
    } }, screen.typeIn, h('button', { type: 'submit' }, 'Type'),
    keyBtn('Return', 'return'), keyBtn('Esc', 'escape'), keyBtn('Tab', 'tab'), keyBtn('⌘ Space', 'space', ['command']));
    wireScreenInput(screen.img);
    screen.el = h('section', {}, h('div', { class: 'row' }, h('h3', { class: 'grow' }, 'Screen'), screen.link, screen.ctl, screen.toggle),
      screen.img, screen.bar, screen.note);
    fetchScreen(k);
  }
  screen.toggle.hidden = !running;
  screen.ctl.hidden = !running || guestOf(v.request) !== 'macos';
  if (!running) { screen.note.textContent = 'The VM is not running, so it has no screen.'; setControl(false); }
  return screen.control && screen.stub ? screen.stub : screen.el;
}

async function fetchScreen(k) {
  clearTimeout(screen.timer);
  if (screen.key !== k) return;
  // The panel leaves the page without stopScreen when the inspector is replaced by another view; stop polling then.
  const again = (ms) => {
    screen.timer = setTimeout(() => { if (screen.el && screen.el.isConnected) fetchScreen(k); else stopScreen(); }, ms);
  };
  if (!screen.running || screen.paused) return;
  if (document.visibilityState !== 'visible') { again(4000); return; }
  const [node, id] = k.split('/');
  const seq = ++screen.seq;
  try {
    const headers = token() ? { Authorization: 'Bearer ' + token() } : {};
    const r = await fetch('/fleet/vms/' + enc(node) + '/' + enc(id) + '/screen?max_width=' + (screen.control ? 1600 : 960), { headers, cache: 'no-store' });
    if (screen.key !== k || seq !== screen.seq) return;
    if (!r.ok) {
      const body = await r.json().catch(() => ({}));
      screen.note.textContent = r.status === 409 ? 'No screen for this VM. ' + (body.error || '')
        : 'The screen could not be loaded: ' + (body.error || r.status + ' ' + r.statusText);
      again(r.status === 409 ? 15000 : 6000);
      return;
    }
    const blob = await r.blob();
    const url = URL.createObjectURL(blob);
    if (screen.url) URL.revokeObjectURL(screen.url);
    screen.url = url;
    screen.img.src = url;
    screen.img.hidden = false;
    screen.link.href = url;
    screen.link.hidden = false;
    const blank = r.headers.get('x-fluxvm-screen-blank') === 'true';
    const w = r.headers.get('x-fluxvm-screen-width');
    const hgt = r.headers.get('x-fluxvm-screen-height');
    screen.note.textContent = screen.control
      ? 'Controlling the VM: clicks, scrolling and keys go to it. Press Stop control when you are done.'
      : (blank ? 'Nothing is drawn yet (the guest is still starting). ' : '')
        + (w && hgt ? w + ' × ' + hgt + '. ' : '') + 'Updates every 2 seconds while this panel is open.';
    again(screen.control ? 700 : 2000);
  } catch (_) {
    if (screen.key === k) { screen.note.textContent = 'Central is not reachable; retrying.'; again(6000); }
  }
}

// ---- snapshots ------------------------------------------------------------------------------------------------------
// A VM's snapshots (taken, restored and deleted from its inspector) and clones (a new VM on the same Mac from a snapshot
// or from the stopped VM's disk).

const SNAPS = {}; // vm key -> { items, limit, at, loading, error }
const SNAP_NAME = '[A-Za-z0-9][A-Za-z0-9._\\-]{0,63}';
const SNAP_KINDS = { 'vm.snapshot': 'Snapshot taken', 'vm.restored': 'VM restored', 'vm.cloned': 'VM cloned' };
const snapPath = (v, rest) => '/fleet/vms/' + enc(v.node) + '/' + enc(v.id) + '/' + rest;
const bytes = (n) => (!n ? '–' : n >= 2 ** 30 ? (n / 2 ** 30).toFixed(n >= 100 * 2 ** 30 ? 0 : 1).replace(/\.0$/, '') + ' GiB'
  : Math.max(1, Math.round(n / 2 ** 20)) + ' MiB');
const pad2 = (n) => String(n).padStart(2, '0');

async function loadSnaps(v) {
  const k = vmKey(v);
  const st = SNAPS[k] = SNAPS[k] || {};
  if (st.loading) return;
  st.loading = true;
  try {
    const r = await api('GET', snapPath(v, 'snapshots'));
    Object.assign(st, { items: r.items || [], limit: r.limit, error: null });
  } catch (e) { st.error = e.message; }
  st.loading = false;
  st.at = Date.now();
  S.lastInspKey = '';
  render();
}

/** The Snapshots and Clone sections of a VM's inspector. */
function snapshotSections(v) {
  const k = vmKey(v);
  const st = SNAPS[k];
  if (!st || (!st.loading && Date.now() - (st.at || 0) > 30000)) loadSnaps(v);
  const items = st && st.items;
  const limit = st && st.limit;
  const full = !!items && limit !== undefined && items.length >= limit;
  const take = h('button', { class: 'plain', disabled: !items || full, title: full ? (limit ? 'This VM keeps at most ' + limit + ' snapshots. Delete one first.' : 'Snapshots are turned off for this VM.') : null,
    onclick: () => snapshotSheet(v) }, 'Take snapshot');
  let body;
  if (!items && st && st.error) {
    body = h('div', {}, h('p', { class: 'small sheet-error' }, 'The snapshots could not be loaded: ' + st.error),
      h('button', { class: 'plain', onclick: () => { st.at = 0; loadSnaps(v); } }, 'Try again'));
  } else if (!items) {
    body = h('p', { class: 'muted small' }, 'Loading…');
  } else if (!items.length) {
    body = h('p', { class: 'muted small' }, 'No snapshots yet. A snapshot keeps this VM\'s disk as it is now, and its memory too if you like, so you can come back to this moment or start new VMs from it.');
  } else {
    body = h('div', { class: 'snaps' }, items.map((s) => h('div', { class: 'snap' },
      h('div', { class: 'row' }, h('b', { class: 'grow snap-name' }, s.name),
        s.memory ? h('span', { class: 'tag accent', title: 'Restoring resumes the VM where it was' }, 'Disk + memory')
          : h('span', { class: 'tag', title: 'Restoring starts the VM from this disk' }, 'Disk')),
      h('div', { class: 'small muted', title: when(s.created_at) }, rel(s.created_at) + ' · ' + bytes(s.size_bytes)),
      can('change') ? h('div', { class: 'row snap-acts' },
        h('button', { class: 'plain', onclick: () => restoreSnapshot(v, s) }, 'Restore…'),
        h('button', { class: 'plain', onclick: () => cloneSheet(v, s.name) }, 'New VM from this…'),
        h('span', { class: 'grow' }),
        h('button', { class: 'plain danger', 'aria-label': 'Delete snapshot ' + s.name, onclick: () => deleteSnapshot(v, s) }, 'Delete…')) : null)));
  }
  const stopped = vmStatus(v).text === 'Stopped';
  return [
    h('section', { id: 'snapshots' }, h('div', { class: 'row', style: { marginBottom: '10px' } }, h('h3', { class: 'grow' }, 'Snapshots'), need('change', take)),
      body,
      items && items.length ? h('p', { class: 'hint' }, items.length + ' of ' + limit + ' kept for this VM.'
        + (pricesSet() && (S.data.prices || {}).snapshot_gib_per_hour ? ' Storage is ' + money(S.data.prices.snapshot_gib_per_hour, null, true) + ' per GiB an hour.' : '')) : null),
    !can('change') ? null : h('section', {}, h('h3', {}, 'Clone'),
      h('p', { class: 'small muted', style: { marginBottom: '10px' } }, stopped
        ? 'A new VM on ' + v.node + ' with a copy of this VM\'s disk, or of one of its snapshots.'
        : 'A new VM on ' + v.node + ' from one of this VM\'s snapshots. To copy its disk as it is now, stop it first.'),
      h('button', { onclick: () => cloneSheet(v) }, 'Clone VM…')),
  ];
}

function snapshotSheet(v) {
  if (!allowed('change')) return;
  const st = vmStatus(v).text;
  const live = st === 'Running' || st === 'Paused';
  const d = new Date();
  const name = 'snap-' + d.getFullYear() + pad2(d.getMonth() + 1) + pad2(d.getDate()) + '-' + pad2(d.getHours()) + pad2(d.getMinutes());
  const note = h('p', { class: 'note' });
  const describeChoice = (mem) => {
    note.textContent = mem
      ? 'The VM pauses while its memory is written out, then carries on. A restore resumes it exactly here.'
      : live ? 'The VM pauses for a moment while its disk is copied. A restore starts it from this disk, like after a power cut.'
        : 'A restore starts the VM from this disk.';
  };
  describeChoice(false);
  const { form } = openSheet({
    title: 'Take a snapshot of ' + v.name,
    intro: 'A copy-on-write copy of the VM, kept on ' + v.node + '. It takes seconds and only uses space as the VM changes afterwards.',
    body: [
      h('label', { class: 'field' }, h('span', {}, 'Name'),
        h('input', { name: 'name', value: name, required: true, pattern: SNAP_NAME, maxlength: '64', autocomplete: 'off', spellcheck: 'false', class: 'mono' }),
        h('div', { class: 'hint' }, 'Something you will recognise later, like before-upgrade. Letters, digits, ".", "_" and "-".')),
      h('div', {}, h('span', { class: 'field-label' }, 'What to keep'),
        h('div', { class: 'choices' },
          choice('memory', 'no', true, 'Disk only', 'Works whether the VM runs or not'),
          choice('memory', 'yes', false, 'Disk and memory', live ? 'Also what is running right now' : 'Needs a running or paused VM', !live))),
      note,
    ],
    submit: 'Take snapshot',
    onSubmit: async (f) => {
      const n = String(f.get('name') || '').trim();
      const mem = f.get('memory') === 'yes';
      await api('POST', snapPath(v, 'snapshots'), { body: { name: n, include_memory: mem } });
      toast('Snapshot ' + n + ' of ' + v.name + ' taken');
      loadSnaps(v);
    },
  });
  form.addEventListener('change', (e) => { if (e.target.name === 'memory') describeChoice(e.target.value === 'yes'); });
}

async function restoreSnapshot(v, s) {
  const ok = await confirmDialog({
    title: 'Restore ' + v.name + ' to ' + s.name + '?',
    body: 'The VM is stopped, its disk is put back to how it was ' + rel(s.created_at) + ' (' + when(s.created_at) + '), and it starts again'
      + (s.memory ? ', resuming right where it was. ' : '. ') + 'Anything written since then is lost; take a snapshot first if you want to keep it.',
    confirm: 'Restore', danger: true,
  });
  if (!ok) return;
  toast('Restoring ' + v.name + '…');
  try {
    await api('POST', snapPath(v, 'snapshots/' + enc(s.name) + '/restore'));
    toast(v.name + ' restored to ' + s.name);
  } catch (e) { toast(e.message, true); }
  refresh({ force: true });
}

async function deleteSnapshot(v, s) {
  await confirmAct({ title: 'Delete snapshot ' + s.name + '?',
    body: v.name + ' can no longer be restored to it, and no new VMs can be made from it. This cannot be undone.',
    confirm: 'Delete snapshot', danger: true },
  'DELETE', snapPath(v, 'snapshots/' + enc(s.name)), 'Snapshot ' + s.name + ' deleted', undefined, () => loadSnaps(v));
}

function cloneSheet(v, snap) {
  if (!allowed('change')) return;
  const items = (SNAPS[vmKey(v)] || {}).items || [];
  const stopped = vmStatus(v).text === 'Stopped';
  const taken = new Set((S.data.vms || []).map((x) => x.name));
  let name = v.name + '-copy';
  for (let i = 2; taken.has(name); i++) name = v.name + '-copy-' + i;
  const pick = snap || (stopped ? '' : items.length ? items[0].name : '');
  const source = h('select', { name: 'source' },
    h('option', { value: '', disabled: !stopped, selected: pick === '' }, stopped ? 'Its disk, as it is now' : 'Its disk now (stop the VM first)'),
    items.map((s) => h('option', { value: s.name, selected: pick === s.name }, 'Snapshot ' + s.name + ' (' + rel(s.created_at) + ')')));
  const macos = guestOf(v.request) === 'macos';
  const mac = macos ? macs().find((m) => m.name === v.node) : null;
  const slotsFull = !!mac && mac.macosGuests >= 2;
  openSheet({
    title: 'New VM from ' + v.name,
    intro: 'It runs on ' + v.node + ' with the same size, settings and networks, and counts against your quota.',
    body: [
      h('label', { class: 'field' }, h('span', {}, 'Name of the new VM'),
        h('input', { name: 'name', value: name, required: true, maxlength: '63', pattern: '[A-Za-z0-9][A-Za-z0-9._\\-]{0,62}', autocomplete: 'off', spellcheck: 'false' })),
      h('label', { class: 'field' }, h('span', {}, 'Copy from'), source,
        h('div', { class: 'hint' }, 'A snapshot can be cloned while the VM runs. Its memory is not copied: the new VM starts from the disk.')),
      !stopped && !items.length ? h('p', { class: 'note warn' }, 'Stop ' + v.name + ', or take a snapshot of it first: a running VM\'s disk changes while it is copied.') : null,
      macos ? h('p', { class: 'note' + (slotsFull ? ' warn' : '') }, slotsFull
        ? v.node + ' already runs two macOS VMs, the most Apple\'s licence allows. Stop or delete one of them first.'
        : 'Apple\'s licence allows two macOS VMs per Mac, so this works only if ' + v.node + ' has a free slot.') : null,
    ],
    submit: 'Create VM',
    onSubmit: async (f) => {
      const n = String(f.get('name') || '').trim();
      const from = String(f.get('source') || '');
      if (!from && !stopped) throw new Error('Stop ' + v.name + ' first, or pick one of its snapshots.');
      const r = await api('POST', snapPath(v, 'clone'), { body: { name: n, snapshot: from || undefined } });
      toast(n + ' created from ' + (from ? 'snapshot ' + from : v.name));
      await refresh({ force: true });
      if (r && r.vm && r.vm.id) go('vms', r.node + '/' + r.vm.id);
    },
  });
}

/** The tenant quota's snapshot field (`f` is quotaFields' field maker). Empty means the policy's limit, not none. */
function snapQuotaField(f, q) {
  const el = f('max_snapshots_per_vm', 'Snapshots per VM', q.max_snapshots_per_vm, 'Empty: the policy\'s limit.');
  el.querySelector('input').placeholder = 'Policy default';
  return el;
}

/** Activity-log wording for snapshot and clone entries; `null` for anything else. */
function describeSnapshot(p, m, target) {
  const [a, b, c, d, e5, f6] = p;
  if (a !== 'vms' || !(d === 'snapshots' || d === 'clone')) return null;
  const vm = vmName(b, c) || c.slice(0, 8);
  if (d === 'clone') return 'Cloned ' + vm + (target ? ' as ' + target : '');
  if (p.length === 4) return 'Took snapshot' + (target ? ' ' + target : '') + ' of ' + vm;
  if (f6 === 'restore') return 'Restored ' + vm + ' to snapshot ' + e5;
  if (m === 'DELETE') return 'Deleted snapshot ' + e5 + ' of ' + vm;
  return null;
}
// ---- tunnel ----------------------------------------------------------------------------------------------------------
// SSH and port tunnels through central: the VM inspector's Connect section, the Tunnels page and the policy fields.

/** The open tunnels to one VM. */
function tunnelsFor(v) {
  return (S.data.tunnels || []).filter((t) => t.node === v.node && t.vm_id === v.id);
}

/** The login name of the VM's image, as `fluxctl fleet ssh` picks it. */
const sshUser = (v) => (imageFor(v) || {}).ssh_user || 'admin';
/** How commands name the VM: its name when the shell and ssh can take it as is, otherwise node/id. */
const vmArg = (v) => (/^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(v.name || '') ? v.name : v.node + '/' + v.id);

function fmtBytes(n) {
  const u = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let v = Number(n) || 0;
  let i = 0;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i += 1; }
  return i ? v.toFixed(1) + ' ' + u[i] : v + ' B';
}
const portsText = (pol) => ((pol.ports || []).length ? pol.ports.join(', ') : 'none');
/** The tunnel rules: from GET /fleet/tunnels, or the policy the operator just saved. */
const tunnelPol = () => ({ ...(S.data.tunnelPolicy || {}), ...((isOp() && S.policy && S.policy.tunnels) || {}) });

/** A labelled command with a Copy button; long commands wrap instead of being cut off. */
function cmdField(label, text, what) {
  const f = copyField(text, what);
  Object.assign(f.querySelector('code').style, { whiteSpace: 'pre-wrap', overflowWrap: 'anywhere', textOverflow: 'clip' });
  return h('div', {}, h('div', { class: 'small muted', style: { marginBottom: '4px' } }, label), f);
}

function connectPanel(v, running) {
  const pol = tunnelPol();
  const open = tunnelsFor(v);
  const head = h('div', { class: 'row' }, h('h3', { class: 'grow' }, 'Connect'),
    open.length ? h('a', { class: 'small', href: '#/tunnels' }, plural(open.length, 'open tunnel')) : null);
  if (pol.enabled === false) {
    return h('section', {}, head, h('p', { class: 'small muted' }, 'Tunnels are turned off in this cloud\'s policy. '
      + (isOp() ? 'Turn them on in Policy.' : 'Ask your operator to turn them on.')));
  }
  const arg = vmArg(v);
  const alias = /^[A-Za-z0-9._-]+$/.test(v.name || '') ? v.name : 'fluxvm-' + v.id.slice(0, 8);
  const config = 'Host ' + alias + '\n'
    + '  User ' + sshUser(v) + '\n'
    + '  HostKeyAlias fluxvm-' + v.id + '\n'
    + '  ProxyCommand fluxctl fleet --central ' + location.origin + ' tunnel-stdio ' + v.node + '/' + v.id + ' %p\n';
  return h('section', {}, head,
    h('p', { class: 'small muted', style: { marginBottom: '12px' } }, 'SSH and port forwards run through central with your '
      + 'token, so the Mac never has to be reachable from your network.'),
    h('div', { class: 'stack' },
      cmdField('SSH', 'fluxctl fleet ssh ' + arg, 'SSH command'),
      cmdField('Forward a port', 'fluxctl fleet port-forward ' + arg + ' 8080:80', 'Port-forward command'),
      cmdField('Set up once', 'export CENTRAL_URL=' + location.origin, 'CENTRAL_URL')),
    h('p', { class: 'hint' }, 'fluxctl reads your token from FLUXVM_AGENT_TOKEN. Ports open to tunnels: ' + portsText(pol) + '.'),
    h('details', { class: 'adv', style: { marginTop: '12px' } },
      h('summary', {}, 'SSH config for scp, rsync and editors'),
      h('pre', { class: 'snippet', style: { whiteSpace: 'pre-wrap', overflowWrap: 'anywhere' } }, config),
      h('div', { class: 'row', style: { marginTop: '8px' } },
        h('span', { class: 'hint grow', style: { marginTop: 0 } }, 'Add it to ~/.ssh/config, then: ssh ' + alias),
        h('button', { class: 'plain', onclick: () => copy(config, 'SSH config') }, 'Copy'))),
    running ? null : h('p', { class: 'hint' }, 'The VM has to be running to connect.'));
}

function closeTunnel(t) {
  return confirmAct({ title: 'Close the tunnel to ' + t.vm_name + ' port ' + t.port + '?',
    body: 'The connection drops at once: an SSH session on it ends and a port forward loses this connection. '
      + 'Nothing changes in the VM, and the client can connect again.', confirm: 'Close tunnel', danger: true },
  'DELETE', '/fleet/tunnels/' + enc(t.id), 'Tunnel to ' + t.vm_name + ' port ' + t.port + ' closed');
}

VIEWS.tunnels = function tunnelsView() {
  const list = [...(S.data.tunnels || [])].sort((a, b) => String(a.tenant || '').localeCompare(String(b.tenant || ''))
    || String(b.opened_at).localeCompare(String(a.opened_at)));
  const pol = tunnelPol();
  const out = [pageHead('Tunnels', isOp() ? 'Every SSH and port tunnel open through this central. Closing one disconnects it at once.'
    : 'Your open SSH and port tunnels to your VMs. They go through central, so no Mac is exposed.')];
  const rules = pol.enabled === false ? 'Tunnels are turned off in the policy.'
    : 'Ports ' + portsText(pol) + '. At most ' + (pol.max_per_tenant || '–') + ' open per tenant; closed after '
      + (pol.idle_timeout_minutes || '–') + ' minutes without traffic.';
  if (!list.length) {
    out.push(empty('No open tunnels', 'Connect to a VM with fluxctl fleet ssh, or forward one of its ports with fluxctl fleet '
      + 'port-forward. Open tunnels appear here while they are in use.', h('button', { class: 'primary', onclick: () => go('vms') }, 'Go to VMs')));
    out.push(h('p', { class: 'hint', style: { marginTop: '12px' } }, rules));
    return out;
  }
  if (isOp()) {
    const per = {};
    for (const t of list) per[t.tenant || ''] = (per[t.tenant || ''] || 0) + 1;
    out.push(h('div', { class: 'row', style: { marginBottom: '12px', flexWrap: 'wrap' } },
      Object.entries(per).map(([k, n]) => h('span', { class: 'tag' + (n >= (pol.max_per_tenant || Infinity) ? ' accent' : '') },
        (k || 'Operator') + ' · ' + n + (pol.max_per_tenant ? ' of ' + pol.max_per_tenant : '')))));
  }
  out.push(h('div', { class: 'panel table-wrap' }, h('table', {},
    h('thead', {}, h('tr', {}, h('th', {}, 'VM'), h('th', {}, 'Port'), isOp() ? h('th', {}, 'Tenant') : null,
      h('th', { class: 'hide-sm' }, 'Opened'), h('th', { class: 'hide-sm' }, 'Idle'), h('th', { class: 'r' }, 'Traffic'), h('th', {}))),
    h('tbody', {}, list.map((t) => h('tr', {},
      h('td', {}, h('a', { href: '#/vms/' + enc(t.node) + '/' + enc(t.vm_id) }, h('b', {}, t.vm_name)), h('span', { class: 'sub' }, t.node)),
      h('td', { class: 'num' }, String(t.port), t.port === 22 ? h('span', { class: 'sub' }, 'SSH') : null),
      isOp() ? h('td', {}, t.tenant || h('span', { class: 'tag' }, 'Operator'), t.key ? h('span', { class: 'sub' }, 'key ' + t.key) : null) : null,
      h('td', { class: 'hide-sm muted', title: when(t.opened_at) }, rel(t.opened_at)),
      h('td', { class: 'hide-sm muted' }, t.idle_seconds < 60 ? 'Active' : dur(t.idle_seconds)),
      h('td', { class: 'r num', title: 'Sent to the VM / received from it' }, '↑ ' + fmtBytes(t.bytes_sent) + '  ↓ ' + fmtBytes(t.bytes_received)),
      h('td', { class: 'r' }, need('admin', h('button', { class: 'plain danger', onclick: () => closeTunnel(t) }, 'Close')))))))));
  out.push(h('p', { class: 'hint', style: { marginTop: '12px' } }, rules));
  return out;
};

/** An activity-log entry about a tunnel (`OPEN`/`CLOSE` on the VM's tunnel path, or a `DELETE` of one). */
function describeTunnel(e, node, id) {
  if (e.path.startsWith('/fleet/tunnels')) return 'Closed a tunnel';
  const t = e.target || ((vmName(node, id) || String(id).slice(0, 8)) + ' tunnel');
  if (e.method === 'CLOSE') {
    const i = t.indexOf(': ');
    return i < 0 ? 'Tunnel to ' + t + ' ended' : 'Tunnel to ' + t.slice(0, i) + ' ended (' + t.slice(i + 2) + ')';
  }
  return (e.status < 400 ? 'Opened a tunnel to ' : 'Was refused a tunnel to ') + t;
}

function tunnelPolicyFields(p) {
  const t = p.tunnels || { enabled: true, ports: ['22', '1024-65535'], idle_timeout_minutes: 60, max_per_tenant: 20 };
  return h('div', { class: 'stack' },
    h('h2', { style: { paddingTop: '12px' } }, 'Tunnels'),
    check('tunnels_enabled', t.enabled !== false, 'Allow SSH and port tunnels to VMs',
      'Tenants reach their VMs\' ports through central with fluxctl fleet ssh and port-forward. The Macs stay unexposed.'),
    h('label', { class: 'field' }, h('span', {}, 'Ports open to tunnels'),
      h('input', { name: 'tunnel_ports', value: (t.ports || []).join(', '), placeholder: '22, 1024-65535', class: 'mono' }),
      h('div', { class: 'hint' }, 'Single ports and ranges, separated by commas.')),
    h('div', { class: 'grid2' },
      h('label', { class: 'field' }, h('span', {}, 'Close idle tunnels after'),
        h('input', { name: 'tunnel_idle', type: 'number', min: '1', max: '1440', value: String(t.idle_timeout_minutes) }),
        h('div', { class: 'hint' }, 'Minutes without traffic either way.')),
      h('label', { class: 'field' }, h('span', {}, 'Open tunnels per tenant'),
        h('input', { name: 'tunnel_max', type: 'number', min: '1', max: '1000', value: String(t.max_per_tenant) }),
        h('div', { class: 'hint' }, 'More are refused until one closes.'))));
}

function readTunnelPolicy(f, p) {
  return {
    ...(p.tunnels || {}),
    enabled: f.has('tunnels_enabled'),
    ports: String(f.get('tunnel_ports') || '').split(/[\s,]+/).map((x) => x.trim()).filter(Boolean),
    idle_timeout_minutes: Number(f.get('tunnel_idle')),
    max_per_tenant: Number(f.get('tunnel_max')),
  };
}

// ---- activity -------------------------------------------------------------------------------------------------------

/** What an activity-log entry did, in words. */
function describe(e) {
  const dec = (x) => { try { return decodeURIComponent(x); } catch (_) { return x; } };
  const p = e.path.split('/').filter(Boolean).map(dec).slice(1); // after "fleet"
  const t = e.target ? ' ' + e.target : '';
  const m = e.method;
  const [a, b, c, d] = p;
  { const snap = describeSnapshot(p, m, e.target); if (snap) return snap; }
  if (a === 'vms' && p.length === 1 && m === 'POST') return 'Launched VM' + t;
  if (a === 'vms' && p.length === 3 && m === 'DELETE') return 'Deleted VM ' + (vmName(b, c) || c.slice(0, 8)) + ' on ' + b;
  if ((a === 'vms' && d === 'tunnel') || a === 'tunnels') return describeTunnel(e, b, c); // ---- tunnel ----
  if (a === 'vms' && d === 'exec') return 'Ran a command in ' + (vmName(b, c) || c.slice(0, 8));
  if (a === 'vms' && ['start', 'stop', 'restart', 'pause', 'resume'].includes(d)) {
    return { start: 'Started ', stop: 'Stopped ', restart: 'Restarted ', pause: 'Paused ', resume: 'Resumed ' }[d] + (vmName(b, c) || c.slice(0, 8));
  }
  if (a === 'leases' && p.length === 1) return 'Leased a Mac' + (e.target ? ' (' + e.target + ')' : '');
  if (a === 'leases' && c === 'release') return 'Released a lease';
  if (a === 'overlays' && p.length === 1) return 'Created network' + t;
  if (a === 'overlays' && c === 'rotate') return 'Rotated the key of ' + b;
  if (a === 'overlays' && c === 'members') return (m === 'DELETE' ? 'Removed ' + d + ' from ' : 'Added' + t + ' to ') + b;
  if (a === 'overlays' && m === 'DELETE') return 'Deleted network ' + b;
  if (a === 'tenants' && p.length === 1) return 'Created tenant' + t;
  if (a === 'tenants' && c === 'keys') return (m === 'DELETE' ? 'Revoked API key ' + d : 'Created API key' + t) + (b === 'me' ? '' : ' for ' + b);
  if (a === 'tenants' && c === 'rotate-token') return 'Issued a new token for ' + b;
  if (a === 'tenants' && c === 'members') return (m === 'POST' ? 'Added member' + t : m === 'PATCH' ? 'Changed the role of ' + d : 'Removed member ' + d) + (b === 'me' ? '' : ' of ' + b);
  if (e.path === '/auth/session') return 'Signed in with SSO';
  if (a === 'tenants' && m === 'PATCH') return 'Changed tenant ' + b;
  if (a === 'tenants' && m === 'DELETE') return 'Deleted tenant ' + b;
  const nodeVerb = { cordon: 'Cordoned ', uncordon: 'Uncordoned ', drain: 'Drained ', scrub: 'Scrubbed ' }[c];
  if (a === 'nodes' && nodeVerb) return nodeVerb + b;
  if (a === 'nodes' && m === 'DELETE') return 'Forgot node ' + b;
  if (a === 'images') return (m === 'DELETE' ? 'Removed image ' : 'Saved image ') + b;
  if (a === 'policy') return 'Changed the policy';
  if (a === 'prices') return 'Changed the price list';
  if (a === 'webhooks') return c === 'test' ? 'Sent a test to a webhook' : m === 'DELETE' ? 'Deleted a webhook' : 'Added a webhook';
  if (a === 'reconcile') return 'Reconciled the fleet';
  if (a === 'pools') return describePool(e, p); // ---- warmpool ----
  if (a === 'volumes') return p.length === 1 ? 'Created volume' + t : c === 'resize' ? 'Resized volume ' + b : 'Deleted volume ' + b;
  if (e.path === '/signup') return 'Signed up' + t;
  if (a === 'billing' && b === 'invoices') return 'Created invoices' + t;
  if (a === 'billing' && b === 'payment-method') return 'Started adding a payment method';
  if (a === 'tenants' && c === 'billing') return 'Changed invoicing for ' + b;
  return m + ' ' + e.path;
}

function vmName(node, id) {
  const v = (S.data.vms || []).find((x) => x.node === node && x.id === id);
  return v && v.name;
}

function activityRows(list) {
  return list.map((e) => {
    const ok = e.status < 400;
    return h('tr', {},
      h('td', { class: 'muted', title: when(e.time) }, rel(e.time)),
      h('td', {}, e.actor === 'operator' ? h('span', { class: 'tag' }, 'Operator') : e.actor,
        e.key ? h('span', { class: 'sub' }, 'key ' + e.key) : null, e.user ? h('span', { class: 'sub' }, e.user) : null),
      h('td', {}, describe(e)),
      h('td', {}, status(ok ? { text: 'Done', cls: 'ok' } : { text: 'Refused (' + e.status + ')', cls: 'bad' })));
  });
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
  refresh();
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
    tenantBillingSection(t),
    h('section', {}, h('div', { class: 'row' }, h('h3', { class: 'grow' }, 'API keys'),
      h('button', { class: 'plain', onclick: () => keySheet(t.id) }, 'Create key')),
    tenantKeys(t.id)),
    h('section', {}, h('div', { class: 'row' }, h('h3', { class: 'grow' }, 'Members'),
      h('button', { class: 'plain', onclick: () => memberSheet(t.id) }, 'Add member')),
    tenantMembers(t.id)),
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

function tenantKeys(id) {
  const keys = S.tenantKeys[id];
  if (!keys) {
    api('GET', '/fleet/tenants/' + enc(id) + '/keys').then((r) => { S.tenantKeys[id] = r.items || []; S.lastInspKey = ''; render(); })
      .catch((e) => { S.tenantKeys[id] = []; toast(e.message, true); });
    return h('p', { class: 'muted small' }, 'Loading…');
  }
  if (!keys.length) return h('p', { class: 'muted small' }, 'Only the primary token.');
  return h('div', { class: 'stack' }, keys.map((k) => {
    const expired = k.expires_at && new Date(k.expires_at).getTime() <= Date.now();
    return h('div', { class: 'row' },
      h('div', { class: 'grow' }, h('div', {}, k.name, ' ', h('span', { class: 'tag' + (k.read_only ? '' : ' accent') }, k.read_only ? 'Read-only' : 'Full')),
        h('div', { class: 'small muted' }, (k.last_used_at ? 'Used ' + rel(k.last_used_at) : 'Never used') + ', '
          + (expired ? 'expired' : k.expires_at ? 'expires ' + rel(k.expires_at) : 'never expires'))),
      h('button', { class: 'plain danger', onclick: () => confirmAct({ title: 'Revoke ' + k.name + '?',
        body: 'Anything using this key stops working at once.', confirm: 'Revoke key', danger: true },
      'DELETE', '/fleet/tenants/' + enc(id) + '/keys/' + enc(k.id), k.name + ' revoked', undefined, () => { delete S.tenantKeys[id]; }) }, 'Revoke'));
  }));
}

function quotaFields(q = {}) {
  const f = (name, label, value, hint) => h('label', { class: 'field' }, h('span', {}, label),
    h('input', { name, type: 'number', min: '0', inputmode: 'numeric', value: value === undefined || value === null ? '' : String(value), placeholder: 'No limit' }),
    hint ? h('div', { class: 'hint' }, hint) : null);
  return h('div', { class: 'grid2' },
    f('max_leased_hosts', 'Macs', q.max_leased_hosts),
    f('max_vms', 'VMs', q.max_vms),
    f('max_vcpus', 'vCPUs', q.max_vcpus),
    f('max_memory_gib', 'Memory (GiB)', q.max_memory_mib ? q.max_memory_mib / 1024 : undefined),
    f('max_volume_gib', 'Volume storage (GiB)', q.max_volume_gib),
    snapQuotaField(f, q));
}

function readQuota(f) {
  const n = (k) => { const v = String(f.get(k) || '').trim(); return v === '' ? undefined : Number(v); };
  const quota = {};
  for (const k of ['max_leased_hosts', 'max_vms', 'max_vcpus', 'max_volume_gib']) if (n(k) !== undefined) quota[k] = n(k);
  if (n('max_memory_gib') !== undefined) quota.max_memory_mib = Math.round(n('max_memory_gib') * 1024);
  if (n('max_snapshots_per_vm') !== undefined) quota.max_snapshots_per_vm = n('max_snapshots_per_vm');
  return quota;
}

// ---- sso ------------------------------------------------------------------------------------------------------------
// Single sign-on: the "Sign in with …" button, the signed-in person in the side footer, roles, and the Members page.
// A console signed in with SSO has no token: the browser sends central's HttpOnly session cookie, and every call
// carries X-FluxVM-Console (central refuses changes made with the cookie without it).

const ROLES = [
  ['viewer', 'Viewer', 'Sees the tenant and its graphs, changes nothing, and does not get live screens or console logs.'],
  ['member', 'Member', 'Launches and manages VMs and networks.'],
  ['admin', 'Admin', 'Also leases Macs and manages keys, members, budget and webhooks.'],
  ['owner', 'Owner', 'Everything, including other owners.'],
];
const roleName = (r) => (ROLES.find((x) => x[0] === r) || [r, r])[1];

/** What the signed-in member may do: 'change' (VMs, networks) or 'admin' (leases, keys, members, budget, webhooks).
 *  A token or the operator may do everything (central decides in the end). */
function can(what) {
  const r = S.user && S.user.role;
  if (!r || r === 'owner' || r === 'admin') return true;
  return r === 'member' && what === 'change';
}
const need = (what, el) => (can(what) ? el : null);
function roleNote(what) {
  return S.user && S.user.role === 'viewer'
    ? 'You are a viewer: you can look around but not change anything. Ask an admin of your team for the member role.'
    : 'Members manage VMs, snapshots, volumes and networks. ' + (what === 'admin' ? 'Leases, warm pools, API keys, members, billing, the budget and webhooks need an admin.' : '');
}
/** Guards an action; explains instead when the role does not allow it. */
function allowed(what) {
  if (can(what)) return true;
  toast(roleNote(what), true);
  return false;
}

async function ssoBoot() {
  try { S.sso = await fetch('/auth/config', { cache: 'no-store' }).then((r) => r.json()); } catch (_) { S.sso = { enabled: false }; }
  const btn = $('signin-sso');
  if (S.sso && S.sso.enabled) {
    btn.hidden = false;
    $('signin-sso-btn').textContent = 'Sign in with ' + (S.sso.provider || 'SSO');
  }
  S.user = null;
  if (S.sso && S.sso.enabled && !token()) {
    try {
      const r = await fetch('/auth/session', { cache: 'no-store' });
      if (r.ok) S.user = await r.json();
    } catch (_) { /* not signed in */ }
  }
}

async function signOut() {
  if (S.user) {
    try { await fetch('/auth/logout', { method: 'POST', headers: { 'X-FluxVM-Console': '1' } }); } catch (_) { /* gone anyway */ }
  }
  storage.del('sessionStorage', TOKEN_KEY);
  location.hash = '';
  location.reload();
}

function renderWho() {
  if (!S.user) return;
  const u = S.user;
  $('who').textContent = u.email;
  $('who').title = u.email;
  const team = u.tenant ? ((u.tenants || []).find((t) => t.id === u.tenant) || {}).display_name || u.tenant : null;
  $('role').textContent = u.operator ? 'Operator, signed in with ' + ((S.sso && S.sso.provider) || 'SSO') : roleName(u.role) + ' · ' + team;
  const switchable = (u.tenants || []).length + (u.can_be_operator ? 1 : 0) > 1;
  $('switch-team').hidden = !switchable;
}

function switchSheet() {
  const u = S.user;
  const opts = (u.tenants || []).map((t) => h('a', { class: 'choice', href: '/auth/login?tenant=' + enc(t.id) },
    h('b', {}, t.display_name || t.id), h('small', {}, roleName(t.role) + (t.id === u.tenant ? ' · current' : ''))));
  if (u.can_be_operator) {
    opts.unshift(h('a', { class: 'choice', href: '/auth/login' }, h('b', {}, 'Operator'), h('small', {}, u.operator ? 'Current' : 'The whole fleet')));
  }
  openSheet({ title: 'Switch team', intro: 'You belong to more than one. Switching signs you in again with ' + ((S.sso && S.sso.provider) || 'SSO')
    + ', usually without asking for your password.', body: [h('div', { class: 'choices' }, opts)], submit: 'Close', cancel: null, onSubmit: async () => {} });
}

/** Members of `owner` ('me' or a tenant id): a table (or a compact list for the details pane), with role changes and
 *  removal when allowed. */
function memberTable(list, owner, compact) {
  const myRole = S.user && S.user.role;
  const full = !S.user || isOp() || myRole === 'owner'; // a token, the operator or an owner
  const manage = isOp() || can('admin');
  const path = (m) => '/fleet/tenants/' + enc(owner) + '/members/' + enc(m.email);
  const after = () => { if (owner === 'me') S.members = null; else delete (S.tenantMembers || {})[owner]; };
  const parts = (m) => {
    const me = S.user && S.user.email === m.email;
    const editable = manage && (full || m.role !== 'owner');
    const role = editable
      ? h('select', { 'aria-label': 'Role of ' + m.email, style: { width: 'auto' }, onchange: (e) => {
        const to = e.target.value;
        doAct('PATCH', path(m), m.email + ' is now ' + roleName(to).toLowerCase(), { role: to }, after);
      } }, ROLES.filter((r) => full || r[0] !== 'owner').map(([v, label]) => h('option', { value: v, selected: v === m.role }, label)))
      : h('span', { class: 'tag' + (m.role === 'owner' || m.role === 'admin' ? ' accent' : '') }, roleName(m.role));
    const remove = editable ? h('button', { class: 'plain danger', onclick: () => confirmAct({
      title: (me ? 'Leave ' : 'Remove ' + m.email + ' from ') + (owner === 'me' ? 'the team' : owner) + '?',
      body: me ? 'You are signed out right away and cannot sign in to this team again unless someone adds you back.'
        : 'They are signed out right away and can no longer sign in. Their past activity stays in the log.',
      confirm: me ? 'Leave team' : 'Remove member', danger: true }, 'DELETE', path(m), m.email + ' removed', undefined, after) }, me ? 'Leave' : 'Remove') : null;
    return { me, role, remove, seen: m.last_sign_in ? 'Signed in ' + rel(m.last_sign_in) : 'Not signed in yet' };
  };
  if (compact) {
    return h('div', { class: 'stack' }, list.map((m) => {
      const x = parts(m);
      return h('div', { class: 'row' }, h('div', { class: 'grow', style: { minWidth: 0 } }, h('div', { style: { overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }, title: m.email }, m.email),
        h('div', { class: 'small muted' }, x.seen)), x.role, x.remove);
    }));
  }
  return h('table', {},
    h('thead', {}, h('tr', {}, h('th', {}, 'Person'), h('th', {}, 'Role'), h('th', { class: 'hide-sm' }, 'Added'),
      h('th', { class: 'hide-sm' }, 'Last sign-in'), h('th', {}))),
    h('tbody', {}, list.map((m) => {
      const x = parts(m);
      return h('tr', {},
        h('td', {}, h('b', {}, m.email), h('span', { class: 'sub' }, x.me ? 'You' : m.added_by ? 'Added by ' + m.added_by : '')),
        h('td', {}, x.role),
        h('td', { class: 'hide-sm muted' }, rel(m.added_at)),
        h('td', { class: 'muted hide-sm' }, m.last_sign_in ? rel(m.last_sign_in) : 'Not yet'),
        h('td', { class: 'r' }, x.remove));
    })));
}

function memberSheet(owner) {
  if (!isOp() && !allowed('admin')) return;
  const full = !S.user || isOp() || S.user.role === 'owner';
  openSheet({
    title: 'Add a member', intro: 'They sign in with ' + ((S.sso && S.sso.enabled && S.sso.provider) || 'single sign-on')
      + ' using this email. Nothing is sent to them: tell them the console address.',
    body: [
      h('label', { class: 'field' }, h('span', {}, 'Work email'), h('input', { name: 'email', type: 'email', required: true, maxlength: '254',
        placeholder: 'alex@example.com', autocomplete: 'off', spellcheck: 'false' }),
      h('div', { class: 'hint' }, 'It has to match the email your identity provider has verified for them.')),
      h('div', { class: 'field' }, h('span', {}, 'Role'), h('div', { class: 'choices' },
        ROLES.map(([v, label, text]) => choice('role', v, v === 'member', label, text, v === 'owner' && !full)))),
    ],
    submit: 'Add member',
    onSubmit: async (f) => {
      const r = await api('POST', '/fleet/tenants/' + enc(owner) + '/members', { body: { email: String(f.get('email')).trim(), role: f.get('role') } });
      if (owner === 'me') S.members = null; else delete (S.tenantMembers || {})[owner];
      toast(r.member.email + ' can now sign in as ' + roleName(r.member.role).toLowerCase());
      refresh({ force: true });
    },
  });
}

function loadMembers() {
  if (S.membersLoading) return;
  S.membersLoading = api('GET', '/fleet/tenants/me/members').then((r) => { S.members = r.items || []; S.membersSso = r.sso; })
    .catch((e) => { S.members = []; toast(e.message, true); })
    .finally(() => { S.membersLoading = null; S.lastViewKey = ''; render(); });
}

VIEWS.members = function membersView() {
  const add = need('admin', h('button', { class: 'primary', onclick: () => memberSheet('me') }, 'Add member'));
  const out = [pageHead('Members', 'The people of your team who sign in to this console, and what each of them may do.', add)];
  if (!S.members) { loadMembers(); out.push(h('p', { class: 'muted' }, 'Loading…')); return out; }
  if (!(S.sso && S.sso.enabled)) {
    out.push(h('p', { class: 'note warn', style: { marginBottom: '16px' } }, 'Single sign-on is not set up on this central yet, so members cannot '
      + 'sign in until your operator configures it. You can add them now.'));
  }
  if (!S.members.length) {
    out.push(empty('Just your token so far', 'Add the people of your team by their work email. They sign in with your company\'s '
      + 'single sign-on and get the role you pick: viewer, member, admin or owner.', add));
    return out;
  }
  out.push(h('div', { class: 'panel table-wrap' }, memberTable(S.members, 'me')));
  out.push(h('div', { class: 'grid2 section' }, ROLES.map(([, label, text]) => h('p', { class: 'small muted' }, h('b', {}, label + ': '), text))));
  return out;
};

/** Operator: a tenant's members in its details pane. */
function tenantMembers(id) {
  S.tenantMembers = S.tenantMembers || {};
  const list = S.tenantMembers[id];
  if (!list) {
    api('GET', '/fleet/tenants/' + enc(id) + '/members').then((r) => { S.tenantMembers[id] = r.items || []; S.lastInspKey = ''; render(); })
      .catch((e) => { S.tenantMembers[id] = []; toast(e.message, true); });
    return h('p', { class: 'muted small' }, 'Loading…');
  }
  if (!list.length) return h('p', { class: 'muted small' }, 'Nobody yet. Add the tenant\'s first owner so they can sign in with SSO.');
  return memberTable(list, id, true);
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
  const kept = volumesOfVm(v).map((x) => x.name);
  await confirmAct({ title: 'Delete ' + v.name + '?', body: 'The VM stops and its disk is deleted. This cannot be undone.'
    + (kept.length === 1 ? ' Its volume ' + kept[0] + ' keeps its data and can be attached to another VM.'
      : kept.length ? ' Its volumes ' + kept.join(', ') + ' keep their data and can be attached to another VM.' : ''),
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

function tokenDialog(id, tok, isKey) {
  openSheet({
    title: (isKey ? 'API key ' : 'Token for ') + id,
    intro: (isKey ? 'Put it in your CI secrets or password manager now.' : 'Send it to the tenant over a private channel.')
      + ' This is the only time it is shown: central keeps only its hash.',
    body: [h('div', { class: 'token-box' }, tok), h('button', { type: 'button', onclick: () => copy(tok, isKey ? 'Key' : 'Token') }, isKey ? 'Copy key' : 'Copy token'),
      h('p', { class: 'small muted' }, isKey ? 'Use it with fluxctl as FLUXVM_AGENT_TOKEN, or sign in to ' + location.origin + '/console with it.'
        : 'The tenant signs in at ' + location.origin + '/console with it.')],
    submit: 'Done', cancel: null, onSubmit: async () => {},
  });
}

function choice(name, value, checked, title, sub, disabled) {
  return h('label', { class: 'choice' }, h('input', { type: 'radio', name, value, checked: !!checked, disabled: !!disabled }),
    h('b', {}, title), sub ? h('small', {}, sub) : null);
}

function leaseSheet(pre = {}) {
  if (!allowed('admin')) return;
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
  const estimate = h('p', { class: 'note' });
  body.push(estimate);
  const showEstimate = (form) => {
    const f = new FormData(form);
    const base = 'A lease is billed for at least 24 hours, even if it is released sooner. Apple\'s macOS licence requires it.';
    if (!pricesSet()) { estimate.textContent = base; return; }
    let rate;
    let exact = true;
    if (op && f.get('node')) {
      const m = avail.find((x) => x.name === f.get('node'));
      rate = m ? macRate(m.node.vcpus_total || 0, m.node.memory_mib_total || 0).rate : 0;
    } else if (op) {
      const rs = avail.map((m) => macRate(m.node.vcpus_total || 0, m.node.memory_mib_total || 0).rate);
      rate = rs.length ? Math.min(...rs) : 0;
    } else {
      const size = f.get('size');
      rate = macRateFrom(size === 'l' ? 16 : size === 'm' ? 8 : 0, size === 'l' ? 48 * 1024 : size === 'm' ? 24 * 1024 : 0);
      exact = false;
    }
    if (!rate || !Number.isFinite(rate)) { estimate.textContent = base; return; }
    const hrs = Number(f.get('hours') || 24);
    estimate.textContent = (exact ? '' : 'From ') + money(rate, null, true) + ' an hour: about ' + money(rate * hrs) + ' for '
      + (f.get('hours') ? hrs + ' hours' : 'the 24-hour minimum, then ' + money(rate * 24) + ' a day until released') + '. ' + base;
  };
  const sheet = openSheet({
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
      const at = r.lease && r.lease.rate_micros ? ' at ' + money(r.lease.rate_micros / 1e6, null, true) + ' an hour' : '';
      toast('Leased ' + node + (op ? ' to ' + b.tenant : '') + at);
      await refresh({ force: true });
      if (node) go('macs', node);
    },
  });
  sheet.form.addEventListener('change', () => showEstimate(sheet.form));
  showEstimate(sheet.form);
}

function launchSheet(pre = {}) {
  if (!allowed('change')) return;
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
  const volPick = volumePicker(pre);

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
    volPick.sync(guest, macSel);
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
    let costLine = '';
    if (pricesSet()) {
      const chosen = macSel.value ? targets.find((m) => m.name === macSel.value) : null;
      const onLease = chosen ? !!chosen.lease : !op && leased.length > 0 && guest === 'macos';
      const maybeShared = !chosen && !(guest === 'macos' && leased.length > 0);
      const p = S.data.prices || {};
      const r = vmRate(c, mem, guest === 'macos');
      if (onLease && !p.charge_vms_on_leased_macs) costLine = ' Included in your lease.';
      else if (r > 0) costLine = ' About ' + money(r, null, true) + ' an hour' + (onLease ? ' on top of the lease.' : maybeShared && leased.length ? ' if it runs on a shared host; free on a Mac you lease.' : '.');
    }
    summary.textContent = (parts.length ? 'Uses ' + parts.join(' and ') + '.' : '') + costLine;
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
    volPick,
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
      if (f.getAll('volume').length) req.volumes = f.getAll('volume');
      const r = await api('POST', '/fleet/vms', { body: req });
      toast(r.warm ? 'Started instantly from warm pool ' + r.pool : 'Launched ' + req.name + ' on ' + r.node);
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
  if (!allowed('change')) return;
  const op = isOp();
  const leased = S.data.leases.filter((l) => l.state === 'active').map((l) => l.node);
  const nodes = op ? (S.data.nodes || []).filter((n) => n.apple).map((n) => n.name) : leased;
  if (!op && !leased.length) {
    toast('Lease a Mac first: a private network connects the Macs you lease', true);
    return;
  }
  const body = [
    h('label', { class: 'field' }, h('span', {}, 'Name'), h('input', { name: 'name', required: true, pattern: '[a-z0-9]([a-z0-9\\-]{0,38}[a-z0-9])?', placeholder: 'backend', autocomplete: 'off' }),
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
      h('label', { class: 'field' }, h('span', {}, 'MTU'), h('input', { name: 'mtu', type: 'number', min: '1280', max: '9000', value: '1400' }),
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
      h('label', { class: 'field' }, h('span', {}, 'SSH user (optional)'), h('input', { name: 'ssh_user', value: i.ssh_user || '', placeholder: 'admin', pattern: '[A-Za-z0-9._][A-Za-z0-9._\\-]{0,31}', autocomplete: 'off' }), h('div', { class: 'hint' }, 'The login name fluxctl fleet ssh uses for VMs from this image. Default: admin.')), // ---- tunnel ----
    ],
    submit: editing ? 'Save changes' : 'Add image',
    onSubmit: async (f) => {
      const s = (k) => { const v = String(f.get(k) || '').trim(); return v || undefined; };
      const body = { guest_os: f.get('guest_os'), image: s('image'), vcpus: Number(f.get('vcpus')),
        memory_mib: Math.round(Number(f.get('memory_gib')) * 1024), display_name: s('display_name'), description: s('description'),
        source: s('source'), disk_size_gib: i.disk_size_gib, ssh_user: s('ssh_user') };
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
      const acts = Array.isArray(r.actions) ? r.actions : [];
      const n = (a) => acts.filter((x) => x.action === a).length;
      const parts = [n('evicted') && n('evicted') + ' deleted', n('rescheduled') && n('rescheduled') + ' moved',
        n('kept') && n('kept') + ' kept running', n('failed') && n('failed') + ' failed'].filter(Boolean);
      toast(m.name + (parts.length ? ' draining: ' + parts.join(', ') : ' cordoned') + (r.done ? '. Nothing left on it.' : ''), n('failed') > 0);
      refresh({ force: true });
    },
  });
}

// ---- volumes --------------------------------------------------------------------------------------------------------
// Persistent volumes: disks that outlive VMs. One lives on one Mac and attaches to one Linux VM at a time, on that Mac.

const volumes = () => S.data.volumes || [];
const volumesOfVm = (v) => volumes().filter((x) => x.attached_to === v.id && x.node === v.node);
const vmOfVolume = (x) => (x.attached_to ? S.data.vms.find((v) => v.id === x.attached_to && v.node === x.node) : null);
const volumeGib = (n) => (n >= 1024 && n % 1024 === 0 ? n / 1024 + ' TiB' : n + ' GiB');
const MAX_VOLUME_GIB = 2048;

function volumeStatus(x) {
  return { creating: { text: 'Creating', cls: 'warn' }, attaching: { text: 'Attaching', cls: 'warn' },
    attached: { text: 'In use', cls: 'ok' } }[x.status] || { text: 'Available', cls: 'idle' };
}

/** A volume's monthly price at today's list, or null when volumes are free. */
function volumeMonthly(gibs) {
  const r = (S.data.prices || {}).volume_gib_per_hour || 0;
  return pricesSet() && r > 0 ? r * gibs * 730 : null;
}

function vmLink(x) {
  const v = vmOfVolume(x);
  if (!x.attached_to) return null;
  return v ? h('a', { href: '#/vms/' + enc(v.node) + '/' + enc(v.id), onclick: (e) => e.stopPropagation() }, v.name)
    : h('code', { title: x.attached_to }, x.attached_to.slice(0, 8));
}

VIEWS.volumes = function volumesView() {
  const list = volumes();
  const create = need('change', h('button', { class: 'primary', onclick: () => volumeSheet() }, 'Create volume'));
  const out = [pageHead('Volumes', 'Disks that keep their data when a VM is deleted. A volume lives on one Mac and serves one '
    + 'Linux VM at a time on that Mac.', create)];
  if (!list.length) {
    out.push(empty('No volumes yet', 'Put a database, a build cache or a home directory on a volume. Delete the VM whenever you '
      + 'like: the data stays, and the next VM you launch with the volume picks up where the last one left off.',
    need('change', h('button', { class: 'primary', onclick: () => volumeSheet() }, 'Create volume'))));
    return out;
  }
  const total = list.reduce((a, x) => a + x.size_gib, 0);
  const q = isOp() ? {} : (S.me.quota || {});
  const inUse = list.filter((x) => x.attached_to).length;
  const monthly = isOp() ? null : volumeMonthly(total);
  out.push(h('div', { class: 'meters', style: { marginBottom: '16px' } },
    isOp() ? h('div', { class: 'panel meter' }, h('div', { class: 'muted small' }, 'Volume storage in the fleet'),
      h('div', { class: 'value' }, volumeGib(total), h('small', {}, ' on ' + plural(new Set(list.map((x) => x.node)).size, 'Mac'))))
      : meter('Your volume storage', total, q.max_volume_gib, volumeGib),
    h('div', { class: 'panel meter' }, h('div', { class: 'muted small' }, 'Attached to a VM'),
      h('div', { class: 'value' }, inUse, h('small', {}, ' of ' + plural(list.length, 'volume')))),
    monthly !== null ? h('div', { class: 'panel meter' }, h('div', { class: 'muted small' }, 'At today\'s prices'),
      h('div', { class: 'value' }, money(monthly), h('small', {}, ' a month'))) : null));
  const sel = route().id;
  out.push(h('div', { class: 'panel table-wrap' }, h('table', {},
    h('thead', {}, h('tr', {}, h('th', {}, 'Name'), h('th', { class: 'r' }, 'Size'), h('th', {}, 'Mac'), h('th', {}, 'Status'),
      h('th', { class: 'hide-sm' }, 'Attached to'), isOp() ? h('th', { class: 'hide-sm' }, 'Tenant') : null,
      h('th', { class: 'hide-sm' }, 'Created'))),
    h('tbody', {}, list.map((x) => h('tr', { class: 'link' + (sel === x.id ? ' selected' : ''), tabindex: '0',
      onclick: () => go('volumes', x.id), onkeydown: (e) => { if (e.key === 'Enter') go('volumes', x.id); } },
    h('td', {}, h('b', {}, x.name), h('span', { class: 'sub mono hide-sm' }, x.id)),
    h('td', { class: 'r num' }, volumeGib(x.size_gib)),
    h('td', {}, x.node),
    h('td', {}, status(volumeStatus(x))),
    h('td', { class: 'hide-sm' }, vmLink(x) || h('span', { class: 'muted' }, '–')),
    isOp() ? h('td', { class: 'hide-sm' }, x.tenant || h('span', { class: 'muted' }, 'operator')) : null,
    h('td', { class: 'hide-sm muted' }, rel(x.created_at))))))));
  out.push(h('p', { class: 'hint', style: { marginTop: '12px' } }, 'A volume shows up in its VM as /dev/disk/by-id/virtio-<ID>. '
    + 'Format it the first time (mkfs.ext4), then mount it.'));
  return out;
};

function volumeInspector(x) {
  const vm = vmOfVolume(x);
  const busy = !!x.attached_to || x.status === 'attaching' || x.status === 'creating';
  const leased = !isOp() && macs().some((m) => m.name === x.node);
  const out = [h('div', { class: 'head' }, h('div', { class: 'grow' }, h('h2', {}, x.name), status(volumeStatus(x))), closeBtn('volumes'))];
  out.push(h('section', {}, facts([
    ['Size', volumeGib(x.size_gib) + ' (' + x.format + ', grows as it fills)'],
    ['Mac', isOp() || leased ? h('a', { href: '#/macs/' + enc(x.node) }, x.node) : x.node],
    ['Attached to', vmLink(x) || 'Nothing'],
    isOp() ? ['Tenant', x.tenant ? h('a', { href: '#/tenants/' + enc(x.tenant) }, x.tenant) : 'operator'] : null,
    ['Created', when(x.created_at)],
    volumeMonthly(x.size_gib) !== null ? ['Costs', 'About ' + money(volumeMonthly(x.size_gib)) + ' a month, attached or not'] : null,
    ['ID', copyField(x.id, 'Volume ID')],
  ])));
  out.push(h('section', {}, h('h3', {}, 'In the guest'),
    copyField('/dev/disk/by-id/virtio-' + x.id, 'Device path'),
    h('p', { class: 'hint' }, 'The first time, make a file system on it (sudo mkfs.ext4 on that path). Every VM after that just mounts it.')));
  if (vm || x.attached_to) {
    out.push(h('section', {}, h('h3', {}, 'In use'),
      h('p', { class: 'small' }, 'Attached to ', vmLink(x), '. Delete that VM to free the volume: it keeps its data.')));
  } else if (x.status === 'available' && can('change')) {
    out.push(h('section', {}, h('button', { class: 'primary', style: { width: '100%' },
      onclick: () => launchSheet({ volume: x.id, image: (S.data.images.find((i) => i.guest_os === 'linux') || {}).name }) }, 'Launch a VM with this volume'),
    h('p', { class: 'hint' }, 'The VM runs on ' + x.node + ', where the volume is. Linux guests only.')));
  }
  if (can('change')) out.push(h('section', {}, h('div', { class: 'actions-col' },
    h('button', { disabled: busy, title: busy ? 'Detach it first: delete the VM using it' : null, onclick: () => resizeVolumeSheet(x) }, 'Resize…'),
    h('button', { class: 'danger', disabled: busy, title: busy ? 'Detach it first: delete the VM using it' : null, onclick: () => deleteVolume(x) }, 'Delete volume…')),
  busy && x.attached_to ? h('p', { class: 'hint' }, 'A volume in use can be neither resized nor deleted.') : null));
  return out;
}

function volumeSheet() {
  if (!allowed('change')) return;
  const op = isOp();
  const q = op ? {} : (S.me.quota || {});
  const held = op ? 0 : volumes().reduce((a, x) => a + x.size_gib, 0);
  const left = q.max_volume_gib === undefined || q.max_volume_gib === null ? null : Math.max(0, q.max_volume_gib - held);
  if (left === 0) { toast('You have used your volume quota of ' + volumeGib(q.max_volume_gib) + '. Delete a volume or ask your operator for more.', true); return; }
  const targets = op ? macs().filter((m) => m.isMac && m.node.healthy !== false && !m.node.cordoned) : macs();
  const sizeIn = h('input', { name: 'size_gib', type: 'number', min: '1', max: String(Math.min(MAX_VOLUME_GIB, left || MAX_VOLUME_GIB)), step: '1', value: String(Math.min(20, left || 20)), required: true });
  const summary = h('p', { class: 'small muted' });
  const showSummary = () => {
    const g = Number(sizeIn.value || 0);
    const parts = [];
    if (left !== null) parts.push('Uses ' + volumeGib(g) + ' of your ' + volumeGib(left) + ' left.');
    const m = volumeMonthly(g);
    if (m !== null) parts.push('About ' + money(m) + ' a month, attached or not.');
    parts.push('Space is only taken on the Mac as the guest writes.');
    summary.textContent = parts.join(' ');
  };
  const body = [
    h('div', { class: 'grid2' },
      h('label', { class: 'field' }, h('span', {}, 'Name'), h('input', { name: 'name', required: true, pattern: '[a-z0-9]([a-z0-9\\-]{0,38}[a-z0-9])?', placeholder: 'pg-data', autocomplete: 'off', spellcheck: 'false' }),
        h('div', { class: 'hint' }, 'Lowercase letters, digits and dashes.')),
      h('label', { class: 'field' }, h('span', {}, 'Size (GiB)'), sizeIn, h('div', { class: 'hint' }, 'Up to ' + volumeGib(MAX_VOLUME_GIB) + '. You can grow it later.'))),
    summary,
    h('label', { class: 'field' }, h('span', {}, 'Mac'), h('select', { name: 'node' },
      h('option', { value: '' }, op ? 'Choose automatically' : 'Choose automatically (a Mac you lease first)'),
      targets.map((m) => h('option', { value: m.name }, m.name + (m.lease && op ? ' (leased to ' + m.lease.tenant + ')' : '')))),
    h('div', { class: 'hint' }, 'A volume stays on this Mac. VMs that use it run here, so pick the Mac you will launch on.')),
    op ? h('label', { class: 'field' }, h('span', {}, 'Tenant'), h('select', { name: 'tenant' },
      h('option', { value: '' }, 'None (an operator volume)'),
      (S.data.tenants || []).filter((t) => !t.disabled).map((t) => h('option', { value: t.id }, t.display_name || t.id))),
    h('div', { class: 'hint' }, 'Only that tenant can attach, resize or delete it.')) : null,
  ];
  const sheet = openSheet({
    title: 'Create a volume', intro: 'A disk that keeps its data when the VM using it is deleted.',
    body, submit: 'Create volume',
    onSubmit: async (f) => {
      const b = { name: String(f.get('name')).trim(), size_gib: Number(f.get('size_gib')) };
      if (f.get('node')) b.node = f.get('node');
      if (op && f.get('tenant')) b.tenant = f.get('tenant');
      const x = await api('POST', '/fleet/volumes', { body: b });
      toast('Created ' + x.name + ' (' + volumeGib(x.size_gib) + ') on ' + x.node);
      await refresh({ force: true });
      go('volumes', x.id);
    },
  });
  sheet.form.addEventListener('input', showSummary);
  showSummary();
}

function resizeVolumeSheet(x) {
  const q = isOp() ? {} : (S.me.quota || {});
  const held = isOp() ? 0 : volumes().reduce((a, v) => a + v.size_gib, 0);
  const room = q.max_volume_gib === undefined || q.max_volume_gib === null ? MAX_VOLUME_GIB : Math.min(MAX_VOLUME_GIB, x.size_gib + q.max_volume_gib - held);
  if (room <= x.size_gib) { toast(x.size_gib >= MAX_VOLUME_GIB ? x.name + ' is already as large as a volume gets.' : 'Your volume quota has no room to grow ' + x.name + '.', true); return; }
  openSheet({
    title: 'Resize ' + x.name, intro: 'Volumes only grow. The data stays where it is.',
    body: [
      h('label', { class: 'field' }, h('span', {}, 'New size (GiB)'),
        h('input', { name: 'size_gib', type: 'number', min: String(x.size_gib + 1), max: String(room), step: '1', value: String(Math.min(room, x.size_gib * 2)), required: true }),
        h('div', { class: 'hint' }, 'Now ' + volumeGib(x.size_gib) + '; at most ' + volumeGib(room) + '.')),
      h('p', { class: 'note' }, 'Next time it is attached, grow the file system inside the guest too: sudo resize2fs on the device (ext4) or xfs_growfs on the mount point (XFS).'),
    ],
    submit: 'Resize volume',
    onSubmit: async (f) => {
      const r = await api('POST', '/fleet/volumes/' + enc(x.id) + '/resize', { body: { size_gib: Number(f.get('size_gib')) } });
      toast(x.name + ' is now ' + volumeGib(r.size_gib));
      refresh({ force: true });
    },
  });
}

async function deleteVolume(x) {
  await confirmAct({ title: 'Delete ' + x.name + '?', body: 'Its ' + volumeGib(x.size_gib) + ' of data are deleted from ' + x.node + '. This cannot be undone.',
    confirm: 'Delete volume', danger: true, type: x.name },
  'DELETE', '/fleet/volumes/' + enc(x.id), x.name + ' deleted', undefined, () => go('volumes'));
}

/** The Launch sheet's volume picker. `sync(guest, macSel)` runs on every change of the sheet. */
function volumePicker(pre) {
  const wrap = h('div');
  const boxes = volumes().map((x) => {
    const free = x.status === 'available';
    const vm = vmOfVolume(x);
    const box = h('input', { type: 'checkbox', name: 'volume', value: x.id, checked: free && (pre.volume === x.id || pre.volume === x.name), disabled: !free });
    box.dataset.node = x.node;
    const sub = free ? volumeGib(x.size_gib) + ' on ' + x.node + (isOp() && x.tenant ? ', ' + x.tenant : '')
      : (x.attached_to ? 'In use by ' + (vm ? vm.name : 'another VM') : 'Not ready yet');
    return { box, row: h('label', { class: 'check' }, box, h('span', {}, x.name, h('span', { class: 'hint', style: { display: 'block' } }, sub))) };
  });
  const note = h('p', { class: 'hint' });
  wrap.sync = (guest, macSel) => {
    const linux = guest === 'linux';
    wrap.hidden = !boxes.length;
    if (!boxes.length) return;
    const picked = boxes.filter((b) => b.box.checked && !b.box.disabled);
    const node = picked.length ? picked[0].box.dataset.node : null;
    for (const b of boxes) {
      if (!linux) b.box.checked = false;
      b.box.disabled = !linux || volumes().find((x) => x.id === b.box.value).status !== 'available' || (node !== null && b.box.dataset.node !== node);
    }
    wrap.replaceChildren(h('span', { class: 'field-label' }, 'Volumes'), note, ...(linux ? boxes.map((b) => b.row) : []));
    if (!linux) { note.textContent = 'Volumes attach to Linux VMs only.'; macSel.disabled = false; return; }
    if (node) {
      macSel.value = node;
      macSel.disabled = true;
      macSel.title = 'Set by the volume: a VM runs on the Mac its volumes are on';
      note.textContent = 'It runs on ' + node + ', where ' + (picked.length === 1 ? 'the volume is' : 'its volumes are') + '. A volume serves one VM at a time.';
    } else {
      macSel.disabled = false;
      macSel.title = '';
      note.textContent = 'Attach a volume to keep data when this VM is deleted.';
    }
  };
  return wrap;
}
// ---- warmpool -------------------------------------------------------------------------------------------------------
// Warm pools (/fleet/pools): VMs booted ahead of time, so a launch of their image starts in seconds. The view, the
// cards' min-ready stepper, the create and launch sheets, and the bits other views use (the VM list's tag, activity).

async function loadPools() {
  const r = await soft(api('GET', '/fleet/pools'), { items: [] });
  return (r.items || []).sort((a, b) => a.name.localeCompare(b.name));
}

/** Pool name -> { min, timer }: a min-ready change shown at once and sent after the clicks settle. */
const poolPending = {};
const poolMine = (p) => isOp() || (S.me && p.tenant === S.me.id);
const MAX_MIN_READY = 32;

function warmTag(v) {
  if (!v.warm_pool) return null;
  const ready = v.warm_state === 'ready';
  return h('span', { class: 'tag warm' + (ready ? ' ready' : ''),
    title: 'Booted ahead of time by warm pool ' + v.warm_pool + '. The next launch of its image takes it.' },
  ready ? 'Warm, ready' : 'Warm, booting');
}

function poolState(p) {
  const st = (p.status || {}).state;
  return { ready: { text: 'Ready', cls: 'ok' }, filling: { text: 'Filling', cls: 'warn' },
    backing_off: { text: 'Retrying soon', cls: 'bad' }, paused: { text: 'Paused', cls: 'idle' } }[st]
    || { text: st || 'Unknown', cls: 'idle' };
}

function poolImageName(p) {
  const img = (S.data.images || []).find((i) => i.name === p.image);
  return (img && (img.display_name || img.name)) || p.image_display_name || p.image;
}

const fmtMs = (ms) => (ms < 1000 ? Math.round(ms) + ' ms' : (ms / 1000).toFixed(ms < 10000 ? 1 : 0) + ' s');

function poolsView() {
  const list = S.data.pools || [];
  const createBtn = () => need('admin', h('button', { class: 'primary', onclick: () => poolSheet() }, 'Create pool'));
  const out = [pageHead('Warm pools', isOp()
    ? 'VMs booted ahead of time. A launch of a pool\'s image takes a ready VM and starts in seconds; the pool then boots a '
      + 'replacement. Shared pools run on Macs nobody leases, a tenant\'s pool on the Macs it leases.'
    : 'VMs booted ahead of time on the Macs you lease. Launch the pool\'s image, or name the pool from CI, and a ready VM '
      + 'starts in seconds; the pool then boots a replacement.', createBtn())];
  if (!list.length) {
    const noImages = !S.data.images.length;
    out.push(empty('No warm pools yet', (isOp()
      ? 'A pool keeps VMs of one image booted and idle, so launches start in seconds instead of waiting for a boot. '
        + 'macOS pools belong on a tenant\'s leased Macs: Apple allows two macOS VMs per Mac.'
      : 'A pool keeps VMs of one image booted and idle on your Macs, so CI jobs start in seconds instead of waiting for '
        + 'macOS to boot. Waiting VMs count against your quota.') + (noImages ? ' First, the catalog needs an image.' : ''),
    noImages ? (isOp() ? h('button', { class: 'primary', onclick: () => imageSheet() }, 'Add image') : null) : createBtn()));
    return out;
  }
  out.push(h('div', { class: 'pools' }, list.map(poolCard)));
  return out;
}

function poolCard(p) {
  const st = p.status || {};
  const mine = poolMine(p) && can('admin');
  const pend = poolPending[p.name];
  const min = pend ? pend.min : p.min_ready;
  const ready = st.ready || 0;
  const booting = st.booting || 0;
  const slots = Math.min(24, Math.max(min, ready + booting));
  const dots = [];
  for (let i = 0; i < slots; i++) {
    const kind = i < ready ? 'ready' : i < ready + booting ? 'booting' : 'slot';
    dots.push(h('span', { class: 'wdot ' + kind, title: { ready: 'Ready to launch', booting: 'Booting', slot: 'Not booted yet' }[kind] }));
  }
  const owner = p.owner === 'operator' ? (isOp() ? 'Shared' : 'Shared by your operator') : (isOp() ? 'Tenant ' + p.owner : 'Yours');
  const stepper = mine ? h('div', { class: 'stepper', role: 'group', 'aria-label': 'VMs to keep ready' },
    h('button', { type: 'button', 'aria-label': 'Keep one fewer ready', disabled: min <= 0, onclick: () => stepPool(p, min - 1) }, '−'),
    h('output', { 'aria-live': 'polite' }, String(min)),
    h('button', { type: 'button', 'aria-label': 'Keep one more ready', disabled: min >= MAX_MIN_READY, onclick: () => stepPool(p, min + 1) }, '+'))
    : h('b', {}, String(min));
  const claims = st.claimed_24h || 0;
  let problem = null;
  if (st.last_error) {
    const retry = st.state === 'backing_off' && st.backoff_until ? 'Retrying ' + rel(st.backoff_until) + '. ' : '';
    problem = h('p', { class: 'note warn', style: { margin: 0 } }, retry + st.last_error[0].toUpperCase() + st.last_error.slice(1) + '.');
  }
  return h('div', { class: 'panel pad pool-card' },
    h('div', { class: 'pool-top' },
      h('div', { class: 'grow' }, h('h3', {}, p.name),
        h('div', { class: 'pool-sub' }, poolImageName(p) + ' · ' + p.size.vcpus + ' vCPUs, ' + gib(p.size.memory_mib))),
      status(poolState(p))),
    h('div', { class: 'warm-row' },
      h('div', { class: 'warm-dots', role: 'img', 'aria-label': ready + ' ready, ' + booting + ' booting, ' + min + ' wanted' }, dots),
      h('span', { class: 'warm-count' }, ready + ' ready' + (booting ? ' · ' + booting + ' booting' : ''))),
    h('div', { class: 'row' },
      h('div', { class: 'grow' }, h('div', { class: 'small' }, 'Keep ready'),
        h('div', { class: 'small muted' }, min === 0 ? 'Paused: nothing is kept booted' : 'Up to ' + Math.max(p.max_total, min) + ' alive at once, launched ones included')),
      stepper),
    h('div', { class: 'pool-stats' },
      h('div', {}, h('b', {}, String(claims)), 'launches in 24 hours'),
      h('div', {}, h('b', {}, st.avg_claim_ms === undefined || st.avg_claim_ms === null ? '–' : fmtMs(st.avg_claim_ms)), 'average start')),
    problem,
    h('div', { class: 'pool-foot' },
      h('span', { class: 'tag' + (p.owner === 'operator' ? '' : ' accent') }, owner),
      h('span', { class: 'grow' }),
      need('change', h('button', { onclick: () => poolLaunchSheet(p) }, 'Launch')),
      mine ? h('button', { class: 'plain danger', onclick: () => deletePool(p) }, 'Delete') : null));
}

function stepPool(p, n) {
  if (n < 0 || n > MAX_MIN_READY) return;
  const pend = poolPending[p.name] || (poolPending[p.name] = {});
  pend.min = n;
  clearTimeout(pend.timer);
  render(true);
  pend.timer = setTimeout(async () => {
    const want = pend.min;
    try {
      await api('PATCH', '/fleet/pools/' + enc(p.name), { body: { min_ready: want } });
      toast(want === 0 ? p.name + ' paused: its waiting VMs are deleted' : p.name + ' keeps ' + plural(want, 'VM') + ' ready');
    } catch (e) { toast(e.message, true); }
    if (poolPending[p.name] === pend) delete poolPending[p.name];
    refresh({ force: true });
  }, 700);
}

async function deletePool(p) {
  const st = p.status || {};
  const warm = (st.ready || 0) + (st.booting || 0);
  await confirmAct({ title: 'Delete warm pool ' + p.name + '?',
    body: (warm ? plural(warm, 'waiting VM') + ' in it ' + (warm === 1 ? 'is' : 'are') + ' deleted. ' : '')
      + 'VMs already launched from it keep running.', confirm: 'Delete pool', danger: true },
  'DELETE', '/fleet/pools/' + enc(p.name), p.name + ' deleted');
}

function poolSheet() {
  if (!allowed('admin')) return;
  const op = isOp();
  const images = S.data.images;
  if (!images.length) {
    toast(op ? 'Add an image to the catalog first: a pool boots VMs of a catalog image' : 'Your operator has not added any images yet', true);
    return;
  }
  const taken = new Set((S.data.pools || []).map((x) => x.name));
  const suggest = (img) => {
    const base = img.name.replace(/[^a-z0-9-]/g, '-').slice(0, 30).replace(/-+$/, '') + '-warm';
    for (let i = 1; ; i++) { const n = i === 1 ? base : base + '-' + i; if (!taken.has(n)) return n; }
  };
  const imgChoices = h('div', { class: 'choices' }, images.map((i, n) => choice('image', i.name, n === 0, i.display_name || i.name,
    guestName(i.guest_os) + ', ' + i.vcpus + ' vCPUs, ' + gib(i.memory_mib))));
  const nameIn = h('input', { name: 'name', required: true, pattern: '[a-z0-9]([a-z0-9\\-]{0,38}[a-z0-9])?', autocomplete: 'off', spellcheck: 'false' });
  let touched = false;
  nameIn.addEventListener('input', () => { touched = true; });
  const cpuIn = h('input', { name: 'vcpus', type: 'number', min: '1', max: '64', required: true });
  const memIn = h('input', { name: 'memory_gib', type: 'number', min: '1', max: '1024', step: '1', required: true });
  const ownerSel = op ? h('select', { name: 'tenant' }, h('option', { value: '' }, 'Shared: Macs nobody leases'),
    (S.data.tenants || []).filter((t) => !t.disabled).map((t) => h('option', { value: t.id }, (t.display_name || t.id) + ': its leased Macs'))) : null;
  const notice = h('div');
  const leased = (S.data.leases || []).some((l) => l.state === 'active');
  let formRef = null;
  const update = (fromImage) => {
    const f = new FormData(formRef.form);
    const img = images.find((i) => i.name === f.get('image'));
    if (!img) return;
    if (fromImage) {
      cpuIn.value = img.vcpus;
      memIn.value = Math.max(1, Math.round(img.memory_mib / 1024));
      if (!touched) nameIn.value = suggest(img);
    }
    const msgs = [];
    const shared = op && !f.get('tenant');
    if (img.guest_os === 'macos') {
      if (!op && !leased) {
        msgs.push(h('div', { class: 'note warn row' }, h('span', { class: 'grow' }, 'macOS VMs run on Macs you lease. The pool fills once you lease one.'),
          h('button', { type: 'button', onclick: () => { $('sheet').close(); leaseSheet(); } }, 'Lease a Mac')));
      }
      if (shared && !(S.policy && S.policy.require_lease_for_macos === false)) {
        msgs.push(h('p', { class: 'note warn' }, 'Under the macOS licence tenants run macOS only on Macs they lease, so they cannot take VMs '
          + 'from a shared macOS pool. Pick a tenant above to give it its own pool, or keep this one for your own launches.'));
      }
      msgs.push(h('p', { class: 'note' }, 'Apple allows two macOS VMs per Mac, and each VM kept ready uses one of them.'));
    } else if (shared) {
      msgs.push(h('p', { class: 'note' }, 'Tenants launching this image on a shared host get a ready VM, billed from the moment they take it.'));
    }
    notice.replaceChildren(...msgs);
  };
  formRef = openSheet({
    title: 'Create a warm pool', wide: true,
    intro: op ? 'Central keeps this many VMs booted and idle. A launch of the image takes one and starts in seconds.'
      : 'Central keeps this many VMs booted and idle on your Macs. A launch of the image takes one and starts in seconds.',
    body: [
      h('div', {}, h('span', { class: 'field-label' }, 'Image'), imgChoices),
      h('label', { class: 'field' }, h('span', {}, 'Name'), nameIn, h('div', { class: 'hint' }, 'Lowercase letters, digits and dashes. CI names the pool with "pool": "<name>".')),
      h('div', { class: 'grid2' },
        h('label', { class: 'field' }, h('span', {}, 'Keep ready'), h('input', { name: 'min_ready', type: 'number', min: '0', max: String(MAX_MIN_READY), value: '1', required: true }),
          h('div', { class: 'hint' }, 'Booted, idle VMs waiting for a launch.')),
        h('label', { class: 'field' }, h('span', {}, 'Most alive at once'), h('input', { name: 'max_total', type: 'number', min: '1', max: '100', placeholder: 'Automatic' }),
          h('div', { class: 'hint' }, 'Waiting plus launched VMs from this pool. Empty: 4 × keep ready.'))),
      h('div', { class: 'grid2' }, h('label', { class: 'field' }, h('span', {}, 'vCPUs'), cpuIn), h('label', { class: 'field' }, h('span', {}, 'Memory (GiB)'), memIn)),
      ownerSel ? h('label', { class: 'field' }, h('span', {}, 'Owner'), ownerSel,
        h('div', { class: 'hint' }, 'A tenant\'s pool counts against its quota and bill; a shared pool\'s VMs are billed to whoever takes one, from then on.')) : null,
      notice,
    ],
    submit: 'Create pool',
    onSubmit: async (f) => {
      const b = { name: String(f.get('name')).trim(), image: f.get('image'), min_ready: Number(f.get('min_ready')),
        vcpus: Number(f.get('vcpus')), memory_mib: Math.round(Number(f.get('memory_gib')) * 1024) };
      if (String(f.get('max_total') || '').trim()) b.max_total = Number(f.get('max_total'));
      if (op && f.get('tenant')) b.tenant = f.get('tenant');
      const r = await api('POST', '/fleet/pools', { body: b });
      toast('Created ' + r.name + (b.min_ready ? ': booting ' + plural(b.min_ready, 'VM') + ' now' : ''));
      if (r.note) setTimeout(() => toast(r.note), 600);
      await refresh({ force: true });
      go('pools');
    },
  });
  formRef.form.addEventListener('change', (e) => update(e.target.name === 'image'));
  update(true);
}

function poolLaunchSheet(p) {
  const st = p.status || {};
  const names = new Set((S.data.vms || []).map((v) => v.name));
  let n = 1;
  while (names.has(p.name + '-' + n)) n++;
  openSheet({
    title: 'Launch from ' + p.name,
    intro: st.ready ? 'A VM is booted and waiting: it is yours in seconds.' : 'No VM is ready right now, so this one boots the usual way.',
    body: [
      facts([['Image', poolImageName(p)], ['Size', p.size.vcpus + ' vCPUs, ' + gib(p.size.memory_mib)], ['Ready now', String(st.ready || 0)]]),
      h('label', { class: 'field' }, h('span', {}, 'Name'), h('input', { name: 'name', required: true, value: p.name + '-' + n,
        pattern: '[A-Za-z0-9][A-Za-z0-9._\\-]{0,62}', autocomplete: 'off', spellcheck: 'false' })),
      h('label', { class: 'field' }, h('span', {}, 'Delete automatically'), h('select', { name: 'ttl' },
        h('option', { value: '' }, 'Never'), h('option', { value: '3600' }, 'After 1 hour'), h('option', { value: '28800' }, 'After 8 hours'),
        h('option', { value: '86400' }, 'After 24 hours'), h('option', { value: '604800' }, 'After 7 days'))),
    ],
    submit: 'Launch VM',
    onSubmit: async (f) => {
      const req = { name: String(f.get('name')).trim(), pool: p.name };
      if (f.get('ttl')) req.ttl_seconds = Number(f.get('ttl'));
      if (isOp() && p.tenant) req.tenant = p.tenant;
      const r = await api('POST', '/fleet/vms', { body: req });
      toast(r.warm ? 'Started instantly from warm pool ' + r.pool : 'Launched ' + req.name + ' on ' + r.node + ' (no warm VM was ready)');
      await refresh({ force: true });
      if (r.vm && r.vm.id) go('vms', r.node + '/' + r.vm.id);
    },
  });
}

/** Activity-log text for /fleet/pools changes and the pool reconciler's own entries. */
function describePool(e, p) {
  const [, b, c] = p;
  const t = e.target;
  if (p.length === 1) return 'Created warm pool' + (t ? ' ' + t : '');
  if (c === 'replace') return 'Replaced a warm VM in ' + b + (t ? ' (' + t + ')' : '');
  if (c === 'fill') return 'Could not boot a warm VM for ' + b + (t ? ': ' + t : '');
  if (c === 'claim') return 'Could not hand over a warm VM from ' + b;
  if (e.method === 'PATCH') return 'Changed warm pool ' + b;
  if (e.method === 'DELETE') return 'Deleted warm pool ' + b;
  return e.method + ' ' + e.path;
}

VIEWS.pools = poolsView;
// ---- signup ---------------------------------------------------------------------------------------------------------
// Self-service sign-up on the sign-in screen; Stripe invoices and payment methods on the Billing page, the tenant
// details and the policy.

S.signup = { cfg: null, cfgAt: 0, invoices: null, invFor: null, invAt: 0, invLoading: false };

/** A call without the bearer token (the sign-up routes need none). */
async function publicCall(method, path, body) {
  let r;
  try {
    r = await fetch(path, { method, cache: 'no-store', headers: body ? { 'Content-Type': 'application/json' } : {},
      body: body ? JSON.stringify(body) : undefined });
  } catch (_) {
    throw new ApiError(0, 'Central is not reachable. Check your connection and try again.');
  }
  const out = await r.json().catch(() => ({}));
  if (!r.ok) throw new ApiError(r.status, (out && out.error) || r.status + ' ' + r.statusText, out);
  return out;
}

/** Shows "Create an account" on the sign-in screen when this central takes sign-ups. */
async function signupOffer() {
  if (!S.signup.cfg || Date.now() - S.signup.cfgAt > 60000) {
    try { S.signup.cfg = await publicCall('GET', '/signup/config'); } catch (_) { S.signup.cfg = { open: false }; }
    S.signup.cfgAt = Date.now();
  }
  $('signup-cta').hidden = !S.signup.cfg.open;
}

const slugify = (s) => String(s).toLowerCase().normalize('NFKD').replace(/[̀-ͯ]/g, '')
  .replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '').slice(0, 40).replace(/-+$/, '');

function signupSheet() {
  const cfg = S.signup.cfg || {};
  const domains = cfg.allowed_email_domains || [];
  let idEdited = false;
  const idInput = h('input', { name: 'tenant_id', required: true, pattern: '[a-z0-9][a-z0-9\\-]{0,62}', maxlength: '63',
    placeholder: 'acme', autocomplete: 'off', spellcheck: 'false', oninput: () => { idEdited = true; } });
  const nameInput = h('input', { name: 'display_name', placeholder: 'Acme Mobile', maxlength: '80', autocomplete: 'organization',
    oninput: (e) => { if (!idEdited) idInput.value = slugify(e.target.value); } });
  openSheet({
    title: 'Create your account',
    intro: 'Lease whole Macs by the day and run macOS and Linux VMs on them, from this console or fluxctl.',
    body: [
      h('label', { class: 'field' }, h('span', {}, 'Company or team'), nameInput),
      h('label', { class: 'field' }, h('span', {}, 'Account ID'), idInput,
        h('div', { class: 'hint' }, 'Lowercase letters, digits and dashes. It is part of URLs and network names and cannot be changed later.')),
      h('label', { class: 'field' }, h('span', {}, 'Work email'),
        h('input', { name: 'email', type: 'email', required: true, maxlength: '254', autocomplete: 'email',
          placeholder: domains.length ? 'you@' + domains[0] : 'you@company.com' }),
        h('div', { class: 'hint' }, domains.length ? 'Sign-up here is for addresses at ' + domains.join(', ') + '.'
          : 'For invoices and notices about your account.')),
      cfg.email_confirmation ? h('p', { class: 'note' }, 'We email you a confirmation link. The account is created, and '
        + 'its access token shown, when you open it.') : null,
      cfg.require_payment_method ? h('p', { class: 'note' }, 'Next, you add a payment method with Stripe. You are billed '
        + 'monthly, only for what you use.') : null,
    ],
    submit: 'Create account',
    onSubmit: async (f) => {
      const body = { tenant_id: String(f.get('tenant_id')).trim(), email: String(f.get('email')).trim() };
      const dn = String(f.get('display_name') || '').trim();
      if (dn) body.display_name = dn;
      let r;
      try {
        r = await publicCall('POST', '/signup', body);
      } catch (e) {
        if (e.status === 409 && /already exists/.test(e.message)) throw new Error('The account ID “' + body.tenant_id + '” is taken. Try another one.');
        if (e.status === 429) throw new Error('Too many sign-ups from your network right now. Try again in an hour.');
        if (e.status === 403 && /closed/.test(e.message)) { S.signup.cfg = { open: false }; $('signup-cta').hidden = true; }
        throw e;
      }
      // With email confirmation (the default) nothing exists yet: the link in the mail creates the account.
      if (r && r.email_confirmation) { setTimeout(() => signupCheckEmail(body.email), 0); return; }
      setTimeout(() => signupDone(r), 0);
    },
  });
}

/** After a sign-up that needs its address confirmed: where to look, and what happens next. */
function signupCheckEmail(email) {
  openSheet({
    title: 'Check your email',
    intro: 'If ' + email + ' can be used, a confirmation link is on its way. It is valid for 24 hours.',
    body: [
      h('p', { class: 'small muted' }, 'Opening the link and confirming creates the account and shows its access token '
        + 'once. Nothing is created until then. If nothing arrives, check spam, or start again: a new link replaces the '
        + 'old one after five minutes.'),
    ],
    submit: 'Done',
    cancel: null,
    onSubmit: async () => {},
  });
}

/** The new account's token, shown once, then straight into the console with it. */
function signupDone(r) {
  const tok = r.token;
  const id = (r.tenant && r.tenant.id) || 'account';
  const ack = h('input', { type: 'checkbox', name: 'ack' });
  let finished = false;
  const { dlg, btn } = openSheet({
    title: 'Your account is ready',
    intro: 'This is your access token. It is shown only this once: central keeps just a fingerprint of it.',
    body: [
      h('div', { class: 'token-box' }, tok),
      h('div', { class: 'row' },
        h('button', { type: 'button', onclick: () => copy(tok, 'Token') }, 'Copy token'),
        h('button', { type: 'button', onclick: () => saveToken(id, tok) }, 'Download')),
      h('p', { class: 'small muted' }, 'Sign in with it here, or use it with fluxctl as FLUXVM_AGENT_TOKEN. Keep it in a '
        + 'password manager: if you lose it, your operator has to issue a new one.'),
      h('label', { class: 'check' }, ack, h('span', {}, 'I have stored my token somewhere safe')),
    ],
    submit: r.payment_method_required ? 'Continue to add a payment method' : 'Continue to the console',
    cancel: null,
    onSubmit: async () => {
      finished = true;
      if (r.payment_method_required) location.hash = '#/billing';
      // Sign in exactly as the form does, so nothing from before carries over.
      $('signin-token').value = tok;
      $('signin-form').requestSubmit();
    },
  });
  btn.disabled = true;
  ack.addEventListener('change', () => { btn.disabled = !ack.checked; });
  // Escape or a click outside must not throw the only copy of the token away: the sheet stays (or comes back) until
  // the token is acknowledged.
  const keep = (e) => { if (e.type === 'cancel' || e.target === dlg) { e.preventDefault(); e.stopImmediatePropagation(); } };
  const reopen = () => {
    if (!finished) { if (!dlg.open) dlg.showModal(); return; }
    dlg.removeEventListener('cancel', keep);
    dlg.removeEventListener('click', keep, true);
    dlg.removeEventListener('close', reopen);
  };
  dlg.addEventListener('cancel', keep);
  dlg.addEventListener('click', keep, true);
  dlg.addEventListener('close', reopen);
}

function saveToken(id, tok) {
  const url = URL.createObjectURL(new Blob(['FluxVM Cloud access token for ' + id + '\n' + location.origin + '/console\n\n' + tok + '\n'], { type: 'text/plain' }));
  const a = h('a', { href: url, download: 'fluxvm-' + id + '-token.txt' });
  document.body.append(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 60000);
}

$('signup-link').addEventListener('click', (e) => { e.preventDefault(); signupSheet(); });

// Back from Stripe Checkout: say what happened and drop the query from the address bar.
(function paymentReturn() {
  const pm = new URLSearchParams(location.search).get('payment_method');
  if (!pm) return;
  history.replaceState(null, '', location.pathname + location.hash);
  setTimeout(() => toast(pm === 'added' ? 'Payment method saved. It shows here once Stripe confirms it, usually within seconds.'
    : 'No payment method was added.', pm !== 'added'), 400);
}());

async function addPaymentMethod(btn) {
  if (btn) btn.disabled = true;
  try {
    const r = await api('POST', '/fleet/billing/payment-method');
    if (!/^https?:\/\//.test(r.url || '')) throw new Error('Stripe did not return a page to open.');
    toast('Opening Stripe…');
    location.assign(r.url);
  } catch (e) {
    toast(e.message, true);
    if (btn) btn.disabled = false;
  }
}

async function loadInvoices() {
  if (S.signup.invLoading) return;
  S.signup.invLoading = true;
  const who = token();
  let next;
  try { next = await api('GET', '/fleet/billing/invoices'); } catch (e) { next = { items: [], billing: { payments: false }, error: e.message }; }
  S.signup.invLoading = false;
  if (token() !== who) return;
  const changed = JSON.stringify(next) !== JSON.stringify(S.signup.invoices) || S.signup.invFor !== who;
  Object.assign(S.signup, { invoices: next, invFor: who, invAt: Date.now() });
  if (changed) { S.lastViewKey = ''; S.lastInspKey = ''; render(); }
}

/** Invoices for whoever is signed in; loads them (again after 30 seconds) in the background. */
function invoicesData() {
  const mine = S.signup.invoices && S.signup.invFor === token();
  if (!mine || Date.now() - S.signup.invAt > 30000) loadInvoices();
  return mine ? S.signup.invoices : null;
}

function invoiceStatus(inv) {
  if (inv.status === 'paid') return { text: 'Paid', cls: 'ok' };
  if (inv.status === 'open' && inv.payment_failed) return { text: 'Payment failed', cls: 'bad' };
  if (inv.status === 'open') return { text: 'Due', cls: 'warn' };
  if (inv.status === 'draft') return { text: 'Being prepared', cls: 'idle' };
  if (inv.status === 'void') return { text: 'Void', cls: 'idle' };
  if (inv.status === 'uncollectible') return { text: 'Uncollectible', cls: 'bad' };
  return { text: String(inv.status || 'Unknown'), cls: 'idle' };
}
const monthName = (m) => new Date(m + '-01T00:00:00Z').toLocaleDateString(undefined, { month: 'long', year: 'numeric', timeZone: 'UTC' });
const invoiceLink = (inv) => (/^https:\/\//.test(inv.hosted_invoice_url || '') ? inv.hosted_invoice_url : null);

function signupBanners() {
  // The Billing page has its own, larger prompt.
  if (isOp() || !S.me || !S.me.payment_method_pending || route().page === 'billing') return [];
  return [h('div', { class: 'banner warn', role: 'status' },
    h('span', { class: 'grow' }, 'Add a payment method to lease Macs and launch VMs. You are billed monthly, only for what you use.'),
    need('admin', h('button', { class: 'primary', onclick: (e) => addPaymentMethod(e.currentTarget) }, 'Add payment method')))];
}

/** Top of the Billing page: the tenant's payment method, or the operator's invoice run. */
function billingTop() {
  const d = invoicesData();
  if (!d) return [];
  const b = d.billing || {};
  if (isOp()) return [opInvoicePanel(d, b)];
  if (!b.payments) return [];
  if (b.payment_method_pending) {
    return [h('div', { class: 'panel pad section pm-panel' }, h('div', { class: 'row', style: { gap: '16px' } },
      h('div', { class: 'grow', style: { minWidth: '240px' } }, h('h2', {}, 'Add a payment method to get started'),
        h('p', { class: 'muted small', style: { marginTop: '4px' } }, 'You need one before you can lease a Mac or launch a VM. Card '
          + 'details go straight to Stripe and never reach this site.')),
      need('admin', h('button', { class: 'primary', onclick: (e) => addPaymentMethod(e.currentTarget) }, 'Add payment method'))))];
  }
  return [h('div', { class: 'panel pad section' }, h('div', { class: 'row', style: { gap: '16px' } },
    h('div', { class: 'grow', style: { minWidth: '240px' } }, h('h2', {}, 'Payment method'),
      h('p', { class: 'muted small', style: { marginTop: '4px' } }, b.has_payment_method
        ? 'Saved with Stripe. Invoices are charged to it automatically.'
        : 'None saved: invoices arrive by email and you pay each one online. Add one to pay automatically.')),
    need('admin', h('button', { onclick: (e) => addPaymentMethod(e.currentTarget) }, b.has_payment_method ? 'Replace…' : 'Add payment method'))))];
}

/** Bottom of the tenant's Billing page: invoices with a Pay link. */
function billingBottom() {
  if (isOp()) return [];
  const d = invoicesData();
  if (!d || !(d.billing || {}).payments) return [];
  const list = d.items || [];
  return [h('div', { class: 'panel section' },
    h('div', { class: 'row', style: { padding: '16px 18px 4px' } }, h('h2', { class: 'grow' }, 'Invoices')),
    list.length ? h('div', { class: 'table-wrap' }, h('table', {},
      h('thead', {}, h('tr', {}, h('th', {}, 'Month'), h('th', {}, 'Status'), h('th', { class: 'r' }, 'Amount'), h('th', { class: 'r' }, ''))),
      h('tbody', {}, list.map((inv) => {
        const link = invoiceLink(inv);
        return h('tr', {},
          h('td', {}, monthName(inv.month), inv.number ? h('span', { class: 'inv-num' }, inv.number) : null),
          h('td', {}, status(invoiceStatus(inv))),
          h('td', { class: 'r num' }, money(inv.amount, inv.currency)),
          h('td', { class: 'r' }, link ? h('a', { class: 'btn small' + (inv.status === 'open' ? ' primary' : ''), href: link, target: '_blank',
            rel: 'noopener noreferrer' }, inv.status === 'open' ? 'Pay' : 'View') : null));
      }))))
      : h('p', { class: 'muted small', style: { padding: '4px 18px 18px' } }, 'No invoices yet. Early each month, last month\'s '
        + 'statement becomes an invoice here, with a link to pay it.'))];
}

function opInvoicePanel(d, b) {
  const cur = new Date().toISOString().slice(0, 7);
  const now = new Date();
  const prev = new Date(Date.UTC(now.getUTCFullYear(), now.getUTCMonth() - 1, 1)).toISOString().slice(0, 7);
  const sel = S.billMonth || cur;
  const month = sel < cur ? sel : prev;
  if (!b.payments) {
    return h('div', { class: 'panel pad section' }, h('h2', {}, 'Invoices'),
      h('p', { class: 'muted small', style: { marginTop: '6px' } }, 'Stripe is not set up, so statements are not invoiced. Start '
        + 'central with --stripe-secret-key-file and --stripe-webhook-secret-file to send monthly invoices (docs/mac-cloud-billing.md).'));
  }
  const tenants = S.data.tenants || [];
  const billed = tenants.filter((t) => t.billing_enabled);
  const recs = (d.items || []).filter((r) => r.month === month);
  const ids = [...new Set([...billed.map((t) => t.id), ...recs.map((r) => r.tenant)])].sort();
  const name = (id) => { const t = tenants.find((x) => x.id === id); return (t && t.display_name) || id; };
  const head = h('div', { class: 'row', style: { padding: '16px 18px 8px', gap: '12px' } },
    h('div', { class: 'grow' }, h('h2', {}, 'Invoices for ' + monthName(month), b.mode === 'test' ? [' ', h('span', { class: 'tag' }, 'Stripe test mode')] : null),
      h('p', { class: 'muted small', style: { marginTop: '2px' } }, sel >= cur ? 'This month is still running, so this is last month.'
        : 'Pick another month above to see its invoices.')),
    h('button', { class: 'primary', disabled: !billed.length, title: billed.length ? null : 'No tenant has invoicing on',
      onclick: () => runInvoices(month) }, 'Create invoices for ' + monthName(month).split(' ')[0]));
  const notes = [];
  if (!b.webhooks) notes.push(h('p', { class: 'note warn', style: { margin: '0 18px 12px' } }, 'No webhook secret is set, so payments '
    + 'and saved cards do not show up here. Add the endpoint ' + location.origin + '/stripe/webhook in Stripe and start central with --stripe-webhook-secret-file.'));
  const table = ids.length ? h('div', { class: 'table-wrap' }, h('table', {},
    h('thead', {}, h('tr', {}, h('th', {}, 'Tenant'), h('th', {}, 'Status'), h('th', { class: 'r' }, 'Amount'), h('th', { class: 'hide-sm' }, 'Details'))),
    h('tbody', {}, ids.map((id) => {
      const r = recs.find((x) => x.tenant === id);
      const link = r && invoiceLink(r);
      return h('tr', { class: 'link', tabindex: '0', onclick: () => go('tenants', id), onkeydown: (e) => { if (e.key === 'Enter') go('tenants', id); } },
        h('td', {}, h('b', {}, name(id)), name(id) !== id ? h('span', { class: 'sub' }, id) : null),
        h('td', {}, status(r ? invoiceStatus(r) : { text: 'Not invoiced', cls: 'idle' })),
        h('td', { class: 'r num' }, r ? money(r.amount, r.currency) : '–'),
        h('td', { class: 'hide-sm small' }, r && r.last_error ? h('span', { style: { color: 'var(--bad)' } }, r.last_error)
          : link ? h('a', { href: link, target: '_blank', rel: 'noopener noreferrer', onclick: (e) => e.stopPropagation() }, r.number || 'Open invoice')
            : h('span', { class: 'muted' }, r ? '' : 'Nothing sent yet')));
    }))))
    : h('p', { class: 'muted small', style: { padding: '0 18px 18px' } }, 'No tenant has invoicing on. Accounts that sign up get it '
      + 'automatically; turn it on for others in their details under Tenants.');
  return h('div', { class: 'panel section' }, head, notes, table,
    h('p', { class: 'hint', style: { padding: '0 18px 16px' } }, b.auto_invoice ? 'Invoices are also created automatically on the 1st of each month.'
      : 'To invoice automatically on the 1st of each month, turn it on in Policy.'));
}

async function runInvoices(month) {
  if (!await confirmDialog({ title: 'Create invoices for ' + monthName(month) + '?',
    body: 'Every tenant with invoicing on gets a Stripe invoice for its ' + monthName(month) + ' statement. Stripe charges saved '
      + 'payment methods and emails everyone else a link to pay. Running this again never bills anyone twice.',
    confirm: 'Create invoices' })) return;
  try {
    const r = await api('POST', '/fleet/billing/invoices?month=' + enc(month));
    const parts = [r.created && plural(r.created, 'invoice') + ' created', r.existing && r.existing + ' already invoiced',
      r.skipped && r.skipped + ' with nothing to bill'].filter(Boolean);
    toast(parts.length ? monthName(month) + ': ' + parts.join(', ') : 'Nobody to invoice for ' + monthName(month));
    if (r.failed) {
      const first = (r.results || []).find((x) => x.outcome === 'failed') || {};
      toast(plural(r.failed, 'invoice') + ' failed (' + (first.tenant || '') + ': ' + (first.reason || 'unknown') + '). Run it again to retry.', true);
    }
  } catch (e) { toast(e.message, true); }
  S.signup.invAt = 0;
  loadInvoices();
}

/** Operator: the tenant's billing in its details. */
function tenantBillingSection(t) {
  const d = invoicesData();
  const b = (d && d.billing) || {};
  const cus = t.stripe_customer_id;
  const pm = t.has_payment_method ? 'Saved' : t.payment_method_pending ? 'Required, not added yet' : 'None';
  return h('section', {}, h('h3', {}, 'Billing'),
    facts([
      t.owner_email ? ['Owner', h('a', { href: 'mailto:' + t.owner_email }, t.owner_email)] : null,
      ['Invoices', t.billing_enabled ? 'Monthly, through Stripe' : 'Off'],
      ['Stripe customer', cus ? (b.dashboard ? h('a', { href: b.dashboard + 'customers/' + enc(cus), target: '_blank', rel: 'noopener noreferrer' }, cus)
        : h('code', {}, cus)) : 'None yet'],
      ['Payment method', pm],
      t.self_service ? ['Account', 'Signed up on its own'] : null,
      t.suspended_for_nonpayment ? ['Suspended', status({ text: 'Unpaid invoice', cls: 'bad' })] : null,
    ]),
    b.payments ? h('div', { class: 'row', style: { marginTop: '10px' } },
      h('button', { class: 'plain', onclick: () => setTenantBilling(t, { enabled: !t.billing_enabled }) },
        t.billing_enabled ? 'Turn invoicing off…' : 'Turn invoicing on'),
      t.payment_method_pending ? h('button', { class: 'plain', onclick: () => setTenantBilling(t, { payment_method_required: false }) },
        'Waive payment method') : null)
      : d ? h('p', { class: 'hint' }, 'Stripe is not set up on this central.') : null);
}

async function setTenantBilling(t, body) {
  if (body.enabled === false && !await confirmDialog({ title: 'Turn invoicing off for ' + (t.display_name || t.id) + '?',
    body: 'Their usage is still metered and shown under Billing, but no Stripe invoices are created for them. Invoices already sent are not changed.',
    confirm: 'Turn invoicing off' })) return;
  const msg = body.enabled === true ? 'Invoicing on for ' + t.id + '; a Stripe customer was set up'
    : body.enabled === false ? 'Invoicing off for ' + t.id : t.id + ' can lease and launch without a payment method';
  await doAct('PUT', '/fleet/tenants/' + enc(t.id) + '/billing', msg, body, () => { S.signup.invAt = 0; });
}

/** Policy: who may sign up, and how invoicing runs. */
function signupPolicyPanel() {
  const p = S.policy;
  const form = h('form', { class: 'panel pad stack section', onsubmit: async (e) => {
    e.preventDefault();
    const f = new FormData(e.target);
    const domains = String(f.get('signup_allowed_email_domains') || '').split(/[\s,]+/)
      .map((x) => x.trim().replace(/^@/, '').toLowerCase()).filter(Boolean);
    const next = {
      ...S.policy,
      signup_enabled: f.has('signup_enabled'),
      signup_require_payment_method: f.has('signup_require_payment_method'),
      signup_allowed_email_domains: domains,
      signup_default_quota: readQuota(f),
      billing_auto_invoice: f.has('billing_auto_invoice'),
      billing_suspend_unpaid: f.has('billing_suspend_unpaid'),
      billing_suspend_after_days: Math.max(1, Number(f.get('billing_suspend_after_days')) || 14),
    };
    const btn = e.submitter;
    btn.disabled = true;
    try {
      S.policy = await api('PUT', '/fleet/policy', { body: next });
      S.signup.cfg = null;
      toast(next.signup_enabled ? 'Saved. Sign-up is open at ' + location.origin + '/console' : 'Saved. Sign-up is closed.');
      render(true);
    } catch (err) { toast(err.message, true); btn.disabled = false; }
  } },
  h('h2', {}, 'Sign-up'),
  check('signup_enabled', p.signup_enabled, 'Anyone can create an account',
    'A “Create an account” link appears on the sign-in page. Each sign-up is a new tenant with the quota below. Central needs --token for this.'),
  check('signup_require_payment_method', p.signup_require_payment_method, 'Require a payment method',
    'New accounts save a card with Stripe before they can lease a Mac or launch a VM. Needs Stripe to be set up.'),
  h('label', { class: 'field' }, h('span', {}, 'Only these email domains'),
    h('input', { name: 'signup_allowed_email_domains', value: (p.signup_allowed_email_domains || []).join(', '), placeholder: 'Any domain',
      autocomplete: 'off', spellcheck: 'false' }),
    h('div', { class: 'hint' }, 'Separate several with commas, e.g. acme.io, example.com. Empty allows any address.')),
  h('div', {}, h('span', { class: 'field-label' }, 'Quota for new accounts'),
    h('p', { class: 'hint', style: { margin: '0 0 8px' } }, 'Raise it per tenant later. An empty field means no limit, which is risky for self-service.'),
    quotaFields(p.signup_default_quota || {})),
  h('h2', { style: { paddingTop: '12px' } }, 'Invoices'),
  check('billing_auto_invoice', p.billing_auto_invoice, 'Invoice automatically on the 1st',
    'Last month\'s statements become Stripe invoices for every tenant with invoicing on. You can always do it by hand on the Billing page.'),
  check('billing_suspend_unpaid', p.billing_suspend_unpaid, 'Suspend tenants with unpaid invoices',
    'A tenant is disabled once an invoice stays unpaid for the days below, and enabled again as soon as it is paid.'),
  h('label', { class: 'field', style: { maxWidth: '260px' } }, h('span', {}, 'Suspend after'),
    h('input', { name: 'billing_suspend_after_days', type: 'number', min: '1', max: '365', value: String(p.billing_suspend_after_days || 14) }),
    h('div', { class: 'hint' }, 'Days after the invoice is sent.')),
  h('div', { class: 'row', style: { paddingTop: '8px' } }, h('span', { class: 'grow' }),
    h('button', { class: 'primary', type: 'submit' }, 'Save sign-up and invoices')));
  return form;
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
  cmds.push({ label: 'Create volume', hint: 'Action', run: () => volumeSheet() });
  cmds.push({ label: 'Create warm pool', hint: 'Action', run: () => poolSheet() }); // ---- warmpool ----
  if (isOp()) {
    cmds.push({ label: 'Create tenant', hint: 'Action', run: tenantSheet });
    cmds.push({ label: 'Add image', hint: 'Action', run: () => imageSheet() });
    cmds.push({ label: 'Reconcile now', hint: 'Action', run: reconcileNow });
  }
  cmds.push({ label: 'Switch theme', hint: 'Action', run: cycleTheme });
  for (const v of S.data.vms) cmds.push({ label: v.name, hint: 'VM on ' + v.node, run: () => go('vms', vmKey(v)) });
  for (const m of macs()) cmds.push({ label: m.name, hint: 'Mac', run: () => go('macs', m.name) });
  for (const n of S.data.overlays) cmds.push({ label: n.name, hint: 'Network', run: () => go('networks', n.name) });
  for (const x of S.data.volumes || []) cmds.push({ label: x.name, hint: 'Volume on ' + x.node, run: () => go('volumes', x.id) });
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
  const label = { system: 'Auto', light: 'Light', dark: 'Dark' }[t];
  $('theme').replaceChildren(icon({ system: 'auto', light: 'sun', dark: 'moon' }[t]), h('span', { class: 'sr' }, 'Theme: ' + label));
  $('theme').title = 'Theme: ' + label + '. Click to change.';
}
function cycleTheme() {
  const order = ['system', 'light', 'dark'];
  const cur = storage.get('localStorage', THEME_KEY) || 'system';
  storage.set('localStorage', THEME_KEY, order[(order.indexOf(cur) + 1) % 3]);
  applyTheme();
}

$('theme').addEventListener('click', cycleTheme);
$('bell').addEventListener('click', (e) => { e.stopPropagation(); toggleBell(); });
// Closes on a click outside it, or on a link inside it (a notification or "All and webhooks").
document.addEventListener('click', (e) => {
  const panel = $('bell-panel');
  if (!panel.hidden && (!panel.contains(e.target) || e.target.closest('a'))) toggleBell(false);
});
document.addEventListener('keydown', (e) => { if (e.key === 'Escape' && !$('bell-panel').hidden) toggleBell(false); });
$('refresh').addEventListener('click', () => refresh({ force: true }));
$('signout').addEventListener('click', signOut);
$('switch-team').addEventListener('click', switchSheet);
$('signin-sso-btn').addEventListener('click', () => { location.href = '/auth/login'; });
$('search').addEventListener('input', (e) => {
  S.query = e.target.value;
  if (route().page !== 'vms' && route().page !== 'activity') go('vms'); else render();
});
$('signin-form').addEventListener('submit', (e) => {
  e.preventDefault();
  const t = $('signin-token').value.trim();
  if (!t) return;
  storage.set('sessionStorage', TOKEN_KEY, t);
  S.user = null; // a token wins over an SSO session
  $('signin-token').value = '';
  // Nothing from the previous token's session carries over.
  Object.assign(S, { role: null, me: null, data: null, error: null, usage: null, policy: null, exec: {}, tenantKeys: {}, billMonth: null, billView: null, billLoading: null, hooks: null, hooksLoading: null, logs: {}, query: '',
    lastViewKey: '', lastInspKey: '', loadedAt: 0 });
  stopScreen();
  $('search').value = '';
  refresh({ force: true });
});
window.addEventListener('scroll', () => $('bar').classList.toggle('scrolled', window.scrollY > 4), { passive: true });

// Icon buttons and the account / mobile menus.
$('bell').prepend(icon('bell'));
$('refresh').append(icon('refresh'));
$('menu-toggle').append(icon('menu'));
$('menu-toggle').addEventListener('click', () => {
  const open = !document.body.classList.contains('nav-open');
  document.body.classList.toggle('nav-open', open);
  $('menu-toggle').setAttribute('aria-expanded', String(open));
  if (open) NAV_GROUPS.forEach((g) => { const m = document.getElementById('mega-' + g.id); if (m) m.hidden = false; });
  else closeNav();
});
function toggleAcct(show) {
  const panel = $('acct-panel');
  show = show === undefined ? panel.hidden : show;
  panel.hidden = !show;
  $('acct').setAttribute('aria-expanded', String(show));
}
$('acct').addEventListener('click', (e) => { e.stopPropagation(); toggleAcct(); });
document.addEventListener('click', (e) => { if (!$('acct-panel').hidden && !e.target.closest('.acct-wrap')) toggleAcct(false); });
document.addEventListener('keydown', (e) => { if (e.key === 'Escape' && !$('acct-panel').hidden) toggleAcct(false); });
window.addEventListener('hashchange', () => { closeNav(); document.body.classList.remove('nav-open'); $('menu-toggle').setAttribute('aria-expanded', 'false'); });

applyTheme();
ssoBoot().then(() => refresh({ force: true }));
