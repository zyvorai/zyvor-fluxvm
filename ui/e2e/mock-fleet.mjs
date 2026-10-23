// Mock fleet API for the UI smoke test (e2e/smoke.cjs). Not a model of the real daemon: it serves plausible shapes.
import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
const dist = fileURLToPath(new URL('../fleet/dist', import.meta.url));
const types = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.svg': 'image/svg+xml' };
const ago = (ms) => new Date(Date.now() - ms).toISOString();
const nodes = [
  { name: 'mac-studio-1', vcpus_total: 24, memory_mib_total: 131072, vm_count: 3, cordoned: false, healthy: true },
  { name: 'mac-mini-2', vcpus_total: 10, memory_mib_total: 32768, vm_count: 1, cordoned: true, healthy: true },
  { name: 'mac-mini-3', vcpus_total: 10, memory_mib_total: 32768, vm_count: 0, healthy: false, outdated: true },
];
const vms = [
  { node: 'mac-studio-1', id: 'a1b2c3d4-1', name: 'ci-macos-14', status: 'running', created_at: ago(3.6e6), request: { vcpus: 6, memory_mib: 12288, tenant: 'acme' } },
  { node: 'mac-studio-1', id: 'a1b2c3d4-2', name: 'build-agent', status: 'paused', created_at: ago(8e7), request: { vcpus: 4, memory_mib: 8192, tenant: 'acme' } },
  { node: 'mac-mini-2', id: 'e5f6-3', name: 'qa-ventura', status: 'failed', created_at: ago(2e8), request: { vcpus: 4, memory_mib: 8192, tenant: 'globex' } },
  { node: 'mac-studio-1', id: 'z9-4', name: 'dev-sandbox', status: 'stopped', created_at: ago(9e8), request: { vcpus: 2, memory_mib: 4096 } },
];
const j = (res, o, c = 200) => { res.writeHead(c, { 'content-type': 'application/json' }); res.end(JSON.stringify(o)); };
http.createServer((req, res) => {
  const u = new URL(req.url, 'http://x');
  const p = u.pathname;
  if (p.startsWith('/fleet/') || p.startsWith('/auth/')) {
    if (p === '/auth/config') return j(res, { enabled: true, provider: 'Okta' });
    if (!/Bearer demo/.test(req.headers.authorization || '')) return j(res, { error: 'missing or invalid bearer token' }, 401);
    const op = !/tenant/.test(req.headers.authorization);
    if (p === '/fleet/tenants/me') return op ? j(res, { error: 'not a tenant' }, 400) : j(res, { id: 'acme' });
    if (p === '/fleet/ha') return j(res, { enabled: true, me: 'https://central-a.example.com', leader: 'https://central-a.example.com', is_leader: true });
    if (p === '/fleet/policy') return j(res, { min_lease_hours: 24, overlay_key_rotation_days: 30, tunnels: { enabled: true, max_per_tenant: 8 } });
    if (p === '/fleet/rebalance/plan') return j(res, { moves: [{ vm: 'build-01', from: 'mac-studio-1', to: 'mac-mini-2', memory_mib: 8192 }], skipped: [] });
    if (p === '/fleet/metrics/summary') return j(res, { scope: 'fleet', nodes_reporting: 3, cpu_percent: 41.5, memory_percent: 78.2, vms: 9, vms_running: 7 });
    if (p === '/fleet/billing/invoices') return j(res, { items: [
      { tenant: 'acme', month: '2026-10', id: 'in_1Q2w3e', number: 'ACME-0002', status: 'open', payment_failed: false, currency: 'USD', amount_minor: 41250, amount: '412.50', hosted_invoice_url: 'https://invoice.stripe.com/i/acct_1/test_YWNjdF8xUTJ3M2VSdFlVa3pBNGFTLF9SYWJjZGVmZ2hpamtsbW5vcHFyc3R1dnd4eXo', created_at: '2026-10-01T00:05:00Z', finalized_at: '2026-10-01T00:06:00Z', paid_at: null, last_error: null },
      { tenant: 'acme', month: '2026-09', id: 'in_1P9x8y', number: 'ACME-0001', status: 'paid', payment_failed: false, currency: 'USD', amount_minor: 38820, amount: '388.20', hosted_invoice_url: null, created_at: '2026-09-01T00:05:00Z', finalized_at: '2026-09-01T00:06:00Z', paid_at: '2026-09-03T10:00:00Z', last_error: null },
    ] });
    if (p === '/fleet/capacity') return j(res, { nodes_total: 3, nodes_healthy: 3, nodes_cordoned: 1, nodes_schedulable: 2, vm_count: 9, vcpus_total: 48, vcpus_used: 39, vcpus_free: 9, memory_mib_total: 196608, memory_mib_used: 90112, memory_mib_free: 106496 });
    if (p === '/fleet/volumes') return j(res, { items: [{ id: 'vol-aaaaaaaaaaaa', name: 'build-cache', tenant: op ? 'acme' : null, node: 'mac-studio-1', size_gib: 100, attached_to: 'vm-1', format: 'raw' }, { id: 'vol-bbbbbbbbbbbb', name: 'scratch', tenant: op ? 'acme' : null, node: 'mac-mini-2', size_gib: 20, attached_to: null }, { id: 'vol-cccccccccccc', name: 'new-one', node: 'mac-mini-2', size_gib: 10, creating: true }] });
    if (p === '/fleet/pools') return j(res, { items: [{ name: 'xcode', image: 'macos-14', size: 3, ready: 2, pending: 1 }] });
    if (p === '/fleet/tunnels') return j(res, { items: [{ id: 'tun_1', tenant: 'acme', node: 'mac-studio-1', vm_id: 'vm-1', vm_name: 'build-01', port: 22, opened_at: ago(600000), idle_seconds: 42, bytes_sent: 18400, bytes_received: 5242880 }], enabled: true, max_per_tenant: 8, idle_timeout_minutes: 30, max_lifetime_minutes: 480, ports: [22, 5900] });
    if (p === '/fleet/enroll/tokens') return j(res, { items: [{ id: 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', created: Math.floor(Date.now() / 1000) - 3600, expires: Math.floor(Date.now() / 1000) + 82800, max_uses: 1, uses: 0, name_pattern: 'mac-*', revoked: false, note: 'rack 4' }, { id: 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', created: Math.floor(Date.now() / 1000) - 864000, expires: null, max_uses: 5, uses: 5, name_pattern: null, revoked: false, note: '' }] });
    if (p === '/fleet/enroll/nodes') return j(res, { items: [{ name: 'mac-studio-1', node_id: 'n1', token_id: 'aaaa', generation: 4, revoked: false, enrolled_at: Math.floor(Date.now() / 1000) - 604800 }, { name: 'mac-mini-3', node_id: 'n3', token_id: 'bbbb', generation: 1, revoked: true, enrolled_at: Math.floor(Date.now() / 1000) - 86400 }] });
    if (p === '/fleet/nodes') return j(res, { items: nodes });
    if (/^\/fleet\/nodes\/[^/]+\/drain$/.test(p)) return j(res, { draining: false, scrub_pending: p.includes('mini-2') });
    if (p === '/fleet/vms') return j(res, { items: vms, unreachable_nodes: [{ node: 'mac-mini-3' }] });
    if (p === '/fleet/leases') return j(res, { items: [{ id: 'l1', node: 'mac-studio-1', tenant: 'acme', state: 'active', started_at: ago(4e7), min_end: new Date(Date.now() + 4.6e7).toISOString() }] });
    if (p === '/fleet/tenants') return j(res, { items: [{ id: 'acme', display_name: 'Acme Inc.', quota: { max_leased_hosts: 2, max_vms: 20 } }, { id: 'globex', display_name: 'Globex', quota: {} }] });
    if (p === '/fleet/images') return j(res, { items: [{ name: 'macos-14', display_name: 'macOS Sonoma', image: '/images/macos14.img', guest_os: 'macos', disk_size_gib: 80 }, { name: 'ubuntu', display_name: 'Ubuntu 24.04', image: '/images/ubuntu.img', guest_os: 'linux' }] });
    if (p === '/fleet/overlays') return j(res, { items: [{ name: 'backend', subnet: '10.77.0.0/24', mtu: 1400, members: { 'mac-studio-1': {}, 'mac-mini-2': {} } }] });
    if (p === '/fleet/events') return j(res, { items: [{ at: ago(30000), action: 'vm.start', actor: 'acme', target: 'ci-macos-14' }, { at: ago(600000), action: 'node.cordon', actor: 'operator', target: 'mac-mini-2' }] });
    if (p === '/fleet/billing') return j(res, { month: '2026-10', total_usd: 412.5, leased_host_hours: 310 });
    if (p === '/fleet/usage') return j(res, { items: [{ tenant: 'acme', vm_hours: 1280, host_hours: 310 }, { tenant: 'globex', vm_hours: 96, host_hours: 0 }] });
    if (p === '/fleet/webhooks') return j(res, { items: [{ id: 'w1', url: 'https://hooks.example.com/fluxvm', events: [] }] });
    if (p === '/fleet/notifications') return j(res, { items: [] });
    if (/\/logs$/.test(p)) { res.writeHead(200, { 'content-type': 'text/plain' }); return res.end('Booting macOS 14.4\nlaunchd: ready\n'); }
    return j(res, { error: 'missing' }, 404);
  }
  if (p === '/console') { res.writeHead(308, { location: '/console/' }); return res.end(); }
  let f = path.join(dist, p.replace(/^\/console\/?/, '') || 'index.html');
  if (!fs.existsSync(f) || fs.statSync(f).isDirectory()) f = path.join(dist, 'index.html');
  res.writeHead(200, { 'content-type': types[path.extname(f)] || 'application/octet-stream' });
  fs.createReadStream(f).pipe(res);
}).listen(Number(process.env.PORT || 7792), () => console.log('mock ready'));
