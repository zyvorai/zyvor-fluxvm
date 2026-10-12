// FluxVM fleet console: leases, VMs, private networks and usage for a tenant; plus nodes, tenants, drain and scrub
// for the operator. Talks only to this central's /fleet API. Everything from the API is rendered with textContent,
// never as HTML.
'use strict';

const TOKEN_KEY = 'fluxvm.fleet.token';
const $ = (id) => document.getElementById(id);
let timer = null;
let role = null; // 'operator' | 'tenant'
let me = null;

function el(tag, attrs = {}, ...children) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === 'class') e.className = v;
    else if (k.startsWith('on')) e.addEventListener(k.slice(2), v);
    else if (k === 'disabled') { if (v) e.disabled = true; }
    else e.setAttribute(k, v);
  }
  for (const c of children) {
    if (c !== null && c !== undefined && c !== false) e.append(c instanceof Node ? c : String(c));
  }
  return e;
}

class Unauthorized extends Error {}

async function api(method, path, { body, text = false } = {}) {
  const headers = {};
  const token = sessionStorage.getItem(TOKEN_KEY);
  if (token) headers.Authorization = 'Bearer ' + token;
  if (body !== undefined) headers['Content-Type'] = 'application/json';
  const r = await fetch(path, {
    method, headers, cache: 'no-store', body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (r.status === 401) throw new Unauthorized();
  if (r.status === 204) return {};
  const out = text ? await r.text() : await r.json().catch(() => ({}));
  if (!r.ok) {
    const msg = (out && out.error) || r.status + ' ' + r.statusText;
    const err = new Error(msg);
    err.status = r.status;
    err.leader = out && out.leader;
    throw err;
  }
  return out;
}

function showError(e) {
  if (e instanceof Unauthorized) {
    $('login').hidden = false;
    $('error').textContent = sessionStorage.getItem(TOKEN_KEY) ? 'token rejected' : '';
    return;
  }
  if (e.status === 503 && e.leader) {
    $('error').textContent = 'this central is a standby; the leader is ' + e.leader;
    return;
  }
  $('error').textContent = String(e.message || e);
}

const pill = (cls, text) => el('span', { class: 'pill ' + cls }, text || cls);
const fmtTime = (t) => (t ? new Date(t).toLocaleString() : '');
const gib = (mib) => (mib === undefined || mib === null ? '' : (mib / 1024).toFixed(mib % 1024 ? 1 : 0) + ' GiB');
const hours = (s) => (s / 3600).toFixed(1);
const empty = (cols, text) => el('tr', {}, el('td', { class: 'empty', colspan: String(cols) }, text));

function fill(tbody, rows, cols, emptyText) {
  const t = $(tbody);
  t.replaceChildren(...(rows.length ? rows : [empty(cols, emptyText)]));
}

async function act(fn, confirmText) {
  if (confirmText && !confirm(confirmText)) return;
  try {
    $('error').textContent = '';
    await fn();
  } catch (e) {
    showError(e);
  }
  await refresh();
}

// ---- who am I ------------------------------------------------------------------------------------------------------

async function detectRole() {
  try {
    me = await api('GET', '/fleet/tenants/me');
    role = 'tenant';
  } catch (e) {
    if (e instanceof Unauthorized) throw e;
    if (e.status === 400) {
      role = 'operator';
      me = null;
    } else {
      throw e;
    }
  }
  $('who').textContent = role === 'tenant' ? 'tenant ' + me.id : 'operator';
  $('nodes-card').hidden = role !== 'operator';
  $('tenants-card').hidden = role !== 'operator';
  $('lease-new').hidden = role !== 'tenant';
  $('logout').hidden = !sessionStorage.getItem(TOKEN_KEY);
}

async function haStatus() {
  try {
    const h = await api('GET', '/fleet/ha');
    if (!h.enabled) {
      $('ha').replaceChildren();
      return;
    }
    $('ha').replaceChildren(h.is_leader ? pill('leader', 'leader') : pill('standby', 'standby · leader ' + (h.leader || '?')));
  } catch (_) {
    $('ha').replaceChildren();
  }
}

// ---- sections -------------------------------------------------------------------------------------------------------

function stat(label, value) {
  return el('div', { class: 'stat' }, el('b', {}, value), el('span', { class: 'muted' }, label));
}

async function summary() {
  $('summary').hidden = false;
  if (role === 'tenant') {
    const q = me.quota || {};
    const u = me.in_use || {};
    const of = (v, max) => (max === undefined || max === null ? String(v) : v + ' / ' + max);
    $('stats').replaceChildren(
      stat('leased Macs', of(u.leased_hosts || 0, q.max_leased_hosts)),
      stat('VMs', of(u.vms || 0, q.max_vms)),
      stat('vCPUs', of(u.vcpus || 0, q.max_vcpus)),
      stat('memory', gib(u.memory_mib || 0) + (q.max_memory_mib ? ' / ' + gib(q.max_memory_mib) : '')),
    );
    return;
  }
  const c = await api('GET', '/fleet/capacity');
  $('stats').replaceChildren(
    stat('nodes healthy', (c.nodes_healthy ?? '?') + ' / ' + (c.nodes_total ?? '?')),
    stat('schedulable', c.nodes_schedulable ?? '?'),
    stat('VMs', c.vm_count ?? '?'),
    stat('free vCPUs', (c.vcpus_free ?? '?') + ' / ' + (c.vcpus_total ?? '?')),
    stat('free memory', gib(c.memory_mib_free) + ' / ' + gib(c.memory_mib_total)),
  );
}

async function nodes(leases) {
  if (role !== 'operator') return;
  const r = await api('GET', '/fleet/nodes');
  const leaseOf = Object.fromEntries(leases.filter((l) => l.state === 'active').map((l) => [l.node, l]));
  const items = (r.items || []).sort((a, b) => a.name.localeCompare(b.name));
  const rows = [];
  for (const n of items) {
    const drain = await api('GET', '/fleet/nodes/' + encodeURIComponent(n.name) + '/drain').catch(() => ({}));
    const state = [n.healthy ? pill('ok', 'healthy') : pill('bad', 'stale')];
    if (n.cordoned) state.push(' ', pill('warn', drain.draining ? 'draining' : 'cordoned'));
    if (drain.scrub_pending) state.push(' ', pill('warn', 'scrub pending'));
    const l = leaseOf[n.name];
    const apple = n.apple
      ? [n.apple.nested_virtualization && 'nested', n.apple.vmnet && 'vmnet', 'macOS ' + (n.apple.macos_guests || 0) + '/2']
          .filter(Boolean).join(' · ')
      : '';
    const name = encodeURIComponent(n.name);
    rows.push(el('tr', {},
      el('td', {}, el('code', {}, n.name)),
      el('td', {}, ...state),
      el('td', {}, l ? l.tenant : el('span', { class: 'muted' }, 'shared')),
      el('td', { class: 'num' }, n.vm_count),
      el('td', { class: 'num' }, n.free_vcpus),
      el('td', { class: 'num' }, gib(n.free_memory_mib)),
      el('td', { class: 'muted' }, apple),
      el('td', { class: 'actions' },
        n.cordoned
          ? el('button', { onclick: () => act(() => api('POST', '/fleet/nodes/' + name + '/uncordon')) }, 'Uncordon')
          : el('button', { onclick: () => act(() => api('POST', '/fleet/nodes/' + name + '/cordon')) }, 'Cordon'),
        el('button', {
          onclick: () => act(() => api('POST', '/fleet/nodes/' + name + '/drain?mode=evict-ephemeral'),
            'Drain ' + n.name + '? It is cordoned and its ephemeral VMs are deleted.'),
          disabled: !!l,
        }, 'Drain'),
        drain.scrub_pending
          ? el('button', {
            class: 'danger',
            onclick: () => act(() => api('POST', '/fleet/nodes/' + name + '/scrub?force=true'),
              'Scrub ' + n.name + '? Every VM left on it is deleted and it becomes available again.'),
          }, 'Scrub')
          : null),
    ));
  }
  fill('nodes', rows, 8, 'No nodes have registered yet.');
}

async function tenants() {
  if (role !== 'operator') return;
  const r = await api('GET', '/fleet/tenants');
  const rows = (r.items || []).map((t) => {
    const q = t.quota || {};
    const quota = [['Macs', q.max_leased_hosts], ['VMs', q.max_vms], ['vCPUs', q.max_vcpus],
      ['memory', q.max_memory_mib === undefined ? undefined : gib(q.max_memory_mib)]]
      .filter(([, v]) => v !== undefined && v !== null).map(([k, v]) => v + ' ' + k).join(' · ') || 'unlimited';
    const id = encodeURIComponent(t.id);
    return el('tr', {},
      el('td', {}, el('code', {}, t.id), t.display_name ? el('span', { class: 'muted' }, ' ' + t.display_name) : null),
      el('td', { class: 'muted' }, quota),
      el('td', {}, t.disabled ? pill('bad', 'disabled') : pill('ok', 'enabled')),
      el('td', { class: 'actions' },
        el('button', {
          onclick: () => act(() => api('PATCH', '/fleet/tenants/' + id, { body: { disabled: !t.disabled } })),
        }, t.disabled ? 'Enable' : 'Disable'),
        el('button', {
          onclick: () => act(async () => showToken(t.id, (await api('POST', '/fleet/tenants/' + id + '/rotate-token')).token),
            'Issue a new token for ' + t.id + '? The old one stops working at once.'),
        }, 'New token'),
        el('button', {
          class: 'danger',
          onclick: () => act(() => api('DELETE', '/fleet/tenants/' + id), 'Delete tenant ' + t.id + '?'),
        }, 'Delete')),
    );
  });
  fill('tenants', rows, 4, 'No tenants yet: the fleet is single-tenant until you create one.');
}

function showToken(id, token) {
  const box = $('tenant-token');
  box.replaceChildren(
    el('div', {}, 'Token for ', el('b', {}, id), ' (shown once; central keeps only its hash):'),
    el('code', {}, token),
  );
  box.hidden = false;
}

async function leases() {
  const r = await api('GET', '/fleet/leases');
  const items = (r.items || []).sort((a, b) => (a.state === b.state ? 0 : a.state === 'active' ? -1 : 1));
  const now = Date.now();
  const rows = items.map((l) => {
    const early = new Date(l.min_end).getTime() > now;
    const id = encodeURIComponent(l.id);
    let release = null;
    if (l.state === 'active') {
      if (role === 'operator') {
        release = el('button', {
          class: 'danger',
          onclick: () => act(() => api('POST', '/fleet/leases/' + id + '/release' + (early ? '?force=true' : '')),
            early ? 'Force-release ' + l.node + ' before its 24-hour minimum? It is still billed to ' + fmtTime(l.min_end) + '.'
              : 'Release ' + l.node + '? The tenant\'s VMs on it are deleted.'),
        }, early ? 'Force release' : 'Release');
      } else {
        release = el('button', {
          class: 'danger',
          disabled: early,
          title: early ? 'available from ' + fmtTime(l.min_end) : '',
          onclick: () => act(() => api('POST', '/fleet/leases/' + id + '/release'),
            'Release ' + l.node + '? Your VMs on it are deleted.'),
        }, 'Release');
      }
    }
    return el('tr', {},
      el('td', {}, el('code', {}, l.node)),
      el('td', {}, l.tenant),
      el('td', {}, fmtTime(l.started_at)),
      el('td', {}, fmtTime(l.min_end)),
      el('td', {}, pill(l.state)),
      el('td', { class: 'actions' }, release),
    );
  });
  fill('leases', rows, 6, role === 'tenant' ? 'No leases. Lease a Mac to run macOS guests on it.' : 'No leases.');
  return items;
}

async function vms() {
  const r = await api('GET', '/fleet/vms');
  const items = (r.items || []).sort((a, b) => String(a.name).localeCompare(String(b.name)));
  const rows = items.map((v) => {
    const req = v.request || {};
    const apple = req.apple || {};
    const guest = apple.guest_os === 'macos' ? 'macOS' : (req.backend === 'vz' ? 'Linux (vz)' : req.backend || '');
    const status = typeof v.status === 'string' ? v.status : JSON.stringify(v.status);
    return el('tr', {},
      el('td', {}, v.name),
      el('td', {}, el('code', {}, v.node)),
      el('td', {}, pill(status)),
      el('td', {}, guest),
      el('td', { class: 'num' }, req.vcpus ?? ''),
      el('td', { class: 'num' }, gib(req.memory_mib)),
      el('td', { class: 'actions' }, el('button', {
        class: 'danger',
        onclick: () => act(() => api('DELETE', '/fleet/vms/' + encodeURIComponent(v.node) + '/' + encodeURIComponent(v.id)),
          'Delete ' + v.name + '? Its disk is removed.'),
      }, 'Delete')),
    );
  });
  fill('vms', rows, 7, 'No VMs.');
  const missing = (r.unreachable_nodes || []).map((u) => u.node);
  $('vms-note').textContent = missing.length ? 'not listed (unreachable): ' + missing.join(', ') : '';
}

async function networks() {
  const r = await api('GET', '/fleet/overlays');
  const rows = (r.items || []).map((n) => {
    const members = Object.entries(n.members || {}).map(([node, [a, b]]) => node + ' (.' + a + '–.' + b + ')').join(', ');
    return el('tr', {},
      el('td', {}, el('code', {}, n.name), role === 'operator' && n.tenant ? el('span', { class: 'muted' }, ' ' + n.tenant) : null),
      el('td', {}, el('code', {}, n.subnet)),
      el('td', { class: 'muted' }, members || 'no Macs yet'),
      el('td', {}, n.mtu),
      el('td', { class: 'actions' }, el('button', {
        title: 'key epoch ' + n.key_epoch + ', set ' + fmtTime(n.key_rotated_at),
        onclick: () => act(() => api('POST', '/fleet/overlays/' + encodeURIComponent(n.name) + '/rotate'),
          'Issue a new key for ' + n.name + '? Member Macs switch over within a minute or two without dropping traffic.'),
      }, 'Rotate key'), el('button', {
        class: 'danger',
        onclick: () => act(() => api('DELETE', '/fleet/overlays/' + encodeURIComponent(n.name)),
          'Delete network ' + n.name + '? Guests on it lose their links to other Macs.'),
      }, 'Delete')),
    );
  });
  fill('nets', rows, 5, 'No networks. Guests join one with apple.networks: [{"name": "…"}].');
}

async function usage() {
  const r = await api('GET', '/fleet/usage');
  const rows = (r.tenants || []).map((t) => el('tr', {},
    el('td', {}, t.tenant || el('span', { class: 'muted' }, '(no tenant)')),
    el('td', { class: 'num' }, hours(t.vm_seconds)),
    el('td', { class: 'num' }, hours(t.vcpu_seconds)),
    el('td', { class: 'num' }, (t.memory_mib_seconds / 1024 / 3600).toFixed(1)),
    el('td', { class: 'num' }, hours(t.macos_vm_seconds)),
    el('td', { class: 'num' }, hours(t.lease_seconds)),
    el('td', { class: 'num' }, hours(t.billable_lease_seconds)),
  ));
  fill('usage', rows, 7, 'No usage in the last 30 days.');
}

async function downloadCsv() {
  try {
    const csv = await api('GET', '/fleet/usage?format=csv', { text: true });
    const url = URL.createObjectURL(new Blob([csv], { type: 'text/csv' }));
    const a = el('a', { href: url, download: 'fluxvm-usage.csv' });
    document.body.append(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(url), 60000);
  } catch (e) {
    showError(e);
  }
}

// ---- refresh loop ---------------------------------------------------------------------------------------------------

async function refresh() {
  clearTimeout(timer);
  // Offer to forget the token even when it fails (a disabled tenant gets 403, not the sign-in form).
  $('logout').hidden = !sessionStorage.getItem(TOKEN_KEY);
  await haStatus();
  try {
    await detectRole();
    $('login').hidden = true;
    const ls = await leases();
    await Promise.all([summary(), nodes(ls), tenants(), vms(), networks(), usage()]);
    $('error').textContent = '';
  } catch (e) {
    showError(e);
  }
  // Another refresh (a button, an action) may have run meanwhile and set its own timer: keep only one loop.
  clearTimeout(timer);
  if ($('auto').checked) timer = setTimeout(refresh, 10000);
}

$('login-form').addEventListener('submit', (ev) => {
  ev.preventDefault();
  const t = $('token').value.trim();
  if (t) sessionStorage.setItem(TOKEN_KEY, t);
  $('token').value = '';
  refresh();
});
$('logout').addEventListener('click', () => {
  sessionStorage.removeItem(TOKEN_KEY);
  location.reload();
});
$('refresh').addEventListener('click', refresh);
$('auto').addEventListener('change', refresh);
$('csv').addEventListener('click', downloadCsv);
$('lease-new').addEventListener('click', () => act(() => api('POST', '/fleet/leases', { body: {} }),
  'Lease a whole Mac for at least 24 hours? You are billed for at least 24 hours even if you release it earlier.'));
$('net-form').addEventListener('submit', (ev) => {
  ev.preventDefault();
  const name = $('net-name').value.trim();
  if (!name) return;
  $('net-name').value = '';
  act(() => api('POST', '/fleet/overlays', { body: { name } }));
});
$('tenant-form').addEventListener('submit', (ev) => {
  ev.preventDefault();
  const id = $('tenant-id').value.trim();
  if (!id) return;
  const max = parseInt($('tenant-hosts').value, 10);
  const quota = Number.isFinite(max) ? { max_leased_hosts: max } : {};
  $('tenant-id').value = '';
  $('tenant-hosts').value = '';
  act(async () => showToken(id, (await api('POST', '/fleet/tenants', { body: { id, quota } })).token));
});

refresh();
