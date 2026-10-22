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
