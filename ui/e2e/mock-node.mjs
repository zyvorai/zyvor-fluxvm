// Mock node API for the UI smoke test (e2e/smoke.cjs). Not a model of the real daemon: it serves plausible shapes.
import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
const dist = fileURLToPath(new URL('../node/dist', import.meta.url));
const types = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.svg': 'image/svg+xml' };
const vms = [
  ['web-01', 'running', 'qemu', '10.0.2.15', 2, 2048, 'shop'], ['db-01', 'running', 'qemu', '10.0.2.16', 4, 8192, 'shop'],
  ['edge-demo', 'stopped', 'qemu', '', 1, 1024], ['build-mac', 'paused', 'vz', '192.168.64.4', 6, 12288],
  ['lab-ebpf', 'failed', 'qemu', '', 2, 4096, '', 'cgroup attach failed: permission denied'], ['agent-7', 'running', 'qemu', '10.0.2.20', 2, 2048],
].map(([name, status, backend, ip, vcpus, mem, stack, error], i) => ({ id: `vm-${i}`, name, status, backend, guest_ip: ip || null, labels: stack ? { 'fluxvm.stack': stack } : {}, error: error || null, request: { vcpus, memory_mib: mem } }));
let tick = 0; const state = {};
http.createServer((req, res) => {
  const u = new URL(req.url, 'http://x');
  if (u.pathname.startsWith('/v1/')) {
    if (req.headers.authorization !== 'Bearer demo') { res.writeHead(401, { 'content-type': 'application/json' }); return res.end('{"error":"nope","code":"UNAUTHORIZED"}'); }
    if (u.pathname === '/v1/vms') { res.writeHead(200, { 'content-type': 'application/json' }); return res.end(JSON.stringify({ items: vms })); }
    const j = (o, c = 200) => { res.writeHead(c, { 'content-type': 'application/json' }); res.end(JSON.stringify(o)); };
    let m;
    if (u.pathname === '/v1/sandboxes' && req.method === 'GET') return j({ items: [{ id: 'sb-1', name: 'sandbox-7c1f', status: 'running', backend: 'qemu', expires_at: new Date(Date.now() + 2.4e6).toISOString() }, { id: 'sb-2', name: 'pr-review-12', status: 'paused', backend: 'qemu', expires_at: null }] });
    if (u.pathname === '/v1/templates') return j({ items: [{ name: 'debian-13' }, { name: 'ubuntu-24.04' }] });
    if (u.pathname === '/v1/agents/sessions' && req.method === 'GET') return j({ items: [{ id: 'a1b2c3d4-0000-0000-0000-000000000001', agent: 'claude-code', state: 'running', created_at: new Date(Date.now() - 6e5).toISOString(), expires_at: new Date(Date.now() + 3e6).toISOString(), policy: { preset: 'developer-safe' }, checkpoints: [{ tag: 'cp-1', memory: true, created_at: new Date(Date.now() - 3e5).toISOString() }] }, { id: 'a1b2c3d4-0000-0000-0000-000000000002', agent: 'build-bot', state: 'expired', created_at: new Date(Date.now() - 9e7).toISOString(), policy: { preset: 'offline' } }] });
    if ((m = u.pathname.match(/^\/v1\/agents\/sessions\/([^/]+)$/))) return j({ id: m[1], agent: 'claude-code', state: 'running', created_at: new Date(Date.now() - 6e5).toISOString(), expires_at: new Date(Date.now() + 3e6).toISOString(), policy: { preset: 'developer-safe' }, workspace: { path: '/workspace', git: 'https://github.com/zyvorai/demo' }, capabilities: { pty: true, snapshot: true, fork: true, memory_checkpoint: true }, checkpoints: [{ tag: 'cp-1', memory: true, created_at: new Date(Date.now() - 3e5).toISOString() }] });
    if (/\/approvals$/.test(u.pathname)) return j({ items: [{ id: 'ap1', kind: 'exec', summary: 'git push origin main', matched: 'git push*', state: 'pending', requested_at: new Date(Date.now() - 20000).toISOString() }, { id: 'ap2', kind: 'exec', summary: 'npm publish', matched: 'npm publish*', state: 'denied', requested_at: new Date(Date.now() - 600000).toISOString() }] });
    state.egress ??= { mode: 'audit', rules: [{ cidr: '10.0.0.0/16', proto: 'tcp', port: 5151, note: 'platform' }, { cidr: '91.189.91.0/24', proto: 'tcp', port: 443, note: 'ubuntu mirror' }] };
    if (/\/network\/egress$/.test(u.pathname)) { if (req.method === 'GET') return j({ vm_id: 'x', policy: state.egress, source: 'vm', running: true }); if (req.method === 'PUT') { let b=''; req.on('data', d => b += d); req.on('end', () => { state.egress = JSON.parse(b); j({ policy: state.egress }); }); return; } if (req.method === 'DELETE') { return j({ policy: state.egress }); } }
    if (/\/network\/egress\/observed$/.test(u.pathname)) return j({ flows: [{ dst: '140.82.112.3', proto: 'tcp', port: 443, packets: 412, bytes: 912000, audit: true }, { dst: '185.125.190.36', proto: 'tcp', port: 443, packets: 96, bytes: 220000, audit: true }, { dst: '1.1.1.1', proto: 'udp', port: 53, packets: 12, bytes: 1800, audit: true }] });
    if (/\/network\/egress\/suggest$/.test(u.pathname)) return j({ rules: [{ cidr: '140.82.112.0/24', proto: 'tcp', port: 443, note: 'learned' }], applied: false });
    if (u.pathname === '/v1/branches') return j({ items: [{ id: '11111111-1111-1111-1111-111111111111', name: 'golden', source_vm: 'vm-0', source_name: 'web-01', parent_branch: null, created_at: new Date(Date.now() - 86400000).toISOString(), labels: { 'fluxvm.dev/keep': 'true' }, vcpus: 2, memory_mib: 2048, state_bytes: 2.1e9 }, { id: '22222222-2222-2222-2222-222222222222', name: 'after-deploy', source_vm: 'vm-0', source_name: 'web-01', parent_branch: '11111111-1111-1111-1111-111111111111', created_at: new Date(Date.now() - 3600000).toISOString(), labels: {}, vcpus: 2, memory_mib: 2048, state_bytes: 2.3e9 }] });
    if (u.pathname === '/v1/volumes') return j({ dir: '/var/lib/fluxvm/volumes', items: [{ name: 'build-cache', path: '/var/lib/fluxvm/volumes/build-cache.raw', format: 'raw', size_gib: 100, size_bytes: 107374182400, allocated_bytes: 4.2e9, attached_to: ['web-01'] }, { name: 'scratch', format: 'raw', size_gib: 20, size_bytes: 21474836480, allocated_bytes: 0, attached_to: [] }] });
    if (u.pathname === '/v1/network/cnp') return j({ items: [{ apiVersion: 'cilium.io/v2', kind: 'CiliumNetworkPolicy', metadata: { name: 'allow-dns', namespace: 'default' }, spec: { endpointSelector: { matchLabels: { role: 'web' } } } }] });
    if (u.pathname === '/v1/network/health') return j({ status: 'ok', programs_loaded: 6, maps: { services: 2, conntrack: 412 } });
    if (u.pathname === '/v1/network/ipam') return j({ pool: '10.200.0.0/16', allocated: 14, free: 4082 });
    if (u.pathname === '/v1/network/identities') return j({ items: [{ identity: 1001, labels: 'role=web', refs: 3 }, { identity: 1002, labels: 'role=db', refs: 1 }] });
    if (u.pathname === '/v1/network/endpoints') return j({ items: [{ id: 'ep-1', vm: 'web-01', ip: '10.20.0.5', identity: 1001 }] });
    if (u.pathname === '/v1/network/ipcache') return j({ items: [{ ip: '10.20.0.5', identity: 1001 }] });
    if (u.pathname === '/v1/netobs/dns') return j({ items: [{ name: 'github.com', type: 'A', answers: 2, vm: 'web-01' }] });
    if (u.pathname === '/v1/network/vnets') return j({ items: [{ config: { vni: 100, cidr: '10.20.0.0/24', gateway: '10.20.0.1', uplink: 'eth0', snat_address: '203.0.113.5', floating_ips: [], endpoints: [] }, local_endpoints: ['10.20.0.5', '10.20.0.6'], remote_endpoints: 3, gateway_mac: '02:00:00:00:00:01' }, { config: { vni: 200, cidr: '10.30.0.0/24', gateway: '10.30.0.1', floating_ips: [], endpoints: [] }, local_endpoints: [], remote_endpoints: 0, gateway_mac: '02:00:00:00:00:02' }] });
    if (u.pathname === '/v1/network/groups') return j({ items: [{ name: 'web', labels: ['role=web'], identity: 1001, priority: 10, description: 'front-end tier', policy: { default_allow: false, allow_cidrs: ['10.0.0.0/16'], allow_ports: ['tcp/443', 'udp/53'] } }, { name: 'lab', labels: [], identity: 1002, priority: 0, policy: { default_allow: true } }] });
    if (u.pathname === '/v1/network/services') return j({ items: [{ name: 'api', vip: '10.96.0.10', port: 443, protocol: 'tcp', mode: 'nat', exposure: 'east-west', algorithm: 'maglev', backends: [{ address: '10.20.0.5', port: 8443, state: 'ready', enabled: true }, { address: '10.20.0.6', port: 8443, state: 'draining', enabled: true }] }, { name: 'dns', vip: '10.96.0.53', port: 53, protocol: 'udp', backends: [{ address: '10.20.0.7', port: 53, state: 'ready', enabled: true }] }] });
    if (u.pathname === '/v1/network/services/status') return j({ xdp_acceleration: true, map_tier: 'large', north_south_interfaces: ['eth0'], interfaces: [] });
    if (u.pathname === '/v1/network/services/health') return j({ backends: [{ service: 'api', backend_index: 0, address: '10.20.0.5', port: 8443, healthy: true }, { service: 'api', backend_index: 1, address: '10.20.0.6', port: 8443, healthy: false, last_error: 'connection refused' }] });
    if (u.pathname === '/v1/network/services/flows') return j({ items: [{ service: 'api', source: '10.20.0.5', destination: '10.96.0.10', destination_port: 443, protocol: 6, verdict: 'allow', reason: '', packets: 412, bytes: 912000 }, { service: 'api', source: '10.20.0.9', destination: '10.96.0.10', destination_port: 443, protocol: 6, verdict: 'drop', reason: 'no backend', packets: 3, bytes: 180 }] });
    if (u.pathname === '/v1/network/egress/defaults') return j({ policy: { mode: 'loopback', rules: [] } });
    if (/\/network\/(flows|drops)$/.test(u.pathname)) return j({ flows: [{ src: '10.0.2.15', dst: '140.82.112.3', port: 443, proto: 'tcp', bytes: 912000 }, { src: '10.0.2.15', dst: '1.1.1.1', port: 53, proto: 'udp', bytes: 1800 }] });
    if (/\/disks$/.test(u.pathname)) return j({ disks: [{ name: 'root', size_gib: 20, path: '/var/lib/fluxvm/vms/web-01/root.qcow2' }, { name: 'data', size_gib: 100, path: '/var/lib/fluxvm/vms/web-01/data.qcow2' }] });
    if (/\/snapshots$/.test(u.pathname)) return j({ items: [{ tag: 'before-upgrade', created_at: new Date(Date.now() - 3.6e6).toISOString(), size_bytes: 1.4e9, has_memory: true }, { tag: 'clean-install', created_at: new Date(Date.now() - 8.6e7).toISOString(), size_bytes: 3e8 }] });
    if (u.pathname === '/v1/backups') return j({ items: [{ name: 'web-01-20261021', size_bytes: 4.2e9, live: true, quiesced: true, created_at: new Date(Date.now() - 7e7).toISOString() }] });
    if (/\/migration\/status$/.test(u.pathname)) return j({ phase: 'active', status: 'active', ram_transferred: 1.2e9, ram_total: 2.1e9, total_time_ms: 8400, downtime_ms: null });
    if (u.pathname === '/v1/netobs/summary') return j({ flows: 18342, dns_queries: 2210, drops: 17, bytes: 912000000 });
    if (u.pathname === '/v1/runtime/capabilities') return j({ backend: 'qemu', kvm: true });
    if (u.pathname === '/v1/images/catalog') return j({ items: [{ name: 'ubuntu-24.04', format: 'qcow2', distro: 'ubuntu', version: '24.04', sha256: 'a3f1c9d2e4b57788aabbccdd', signature_valid: true, read_only: true }, { name: 'debian-12', format: 'qcow2', distro: 'debian', version: '12', sha256: '9e8d7c6b5a4f33221100ffee', signature_valid: null, read_only: false }] });
    if (u.pathname === '/v1/oci/images') return j({ items: [{ reference: 'alpine:3.22', digest: 'sha256:8a1b2c3d4e5f60718293a4b5', last_used: new Date(Date.now() - 3.6e6).toISOString() }] });
    if (u.pathname === '/v1/vm-templates') return j({ items: [{ name: 'web-small', description: '2 vCPU, 2 GiB, ubuntu', created_at: new Date(Date.now() - 8.6e7).toISOString(), spec: {} }, { name: 'ci-runner', description: null, created_at: new Date(Date.now() - 2e8).toISOString(), spec: {} }] });
    if (/^\/v1\/vm-templates\//.test(u.pathname)) return j({ name: 'web-small', spec: { name: 'x', image: 'ubuntu-24.04' } });
    if (u.pathname === '/v1/pools') return j({ items: [{ name: 'warm-web', size: 4, ready: 3, pending: 1 }, { name: 'ci', size: 2, ready: 0, pending: 2 }] });
    if (u.pathname === '/v1/host/memory') return j({ total_mib: 65536, available_mib: 41200, level: 'normal' });
    if (u.pathname === '/v1/host/pressure') return j({ cpu: { some_avg10: 0.4 }, memory: { some_avg10: 0.0 }, io: { some_avg10: 1.2 } });
    if (u.pathname === '/v1/host/topology') return j({ numa_nodes: 2, cpus: 32, threads_per_core: 2 });
    if (u.pathname === '/v1/host/confidential') return j({ sev: false, tdx: false });
    if (u.pathname === '/v1/host/hibernation') return j({ facts: { qemu_version: '9.2.0', machine_type: 'q35', cpu_vendor: 'GenuineIntel' } });
    if (u.pathname === '/v1/host/gpus') return j({ items: [{ bdf: '0000:41:00.0', vendor: 'NVIDIA', driver: 'nvidia', iommu_group: 28, iommu_members: ['0000:41:00.0', '0000:41:00.1'], group_bound_to_vfio: false, group_held: false }, { bdf: '0000:81:00.0', vendor: 'NVIDIA', driver: 'vfio-pci', iommu_group: 62, iommu_members: ['0000:81:00.0'], group_bound_to_vfio: true, group_held: true }] });
    if (u.pathname === '/v1/host/gpus/preflight') return j({ iommu_enabled: true, vfio_loaded: true, warnings: ['0000:41:00.0 drives the console'] });
    if (u.pathname === '/v1/host/gpus/health') return j({ items: [{ bdf: '0000:41:00.0', temp_c: 54, util_pct: 3, mem_used_mib: 410 }] });
    if (u.pathname === '/v1/host/mdev') return j({ types: [] });
    if (u.pathname === '/v1/quotas/me') return j({ tenant: 'default', vms: { used: 7, limit: 50 }, vcpus: { used: 18, limit: 128 } });
    if (u.pathname === '/v1/security/capabilities') return j({ guard: true, attestation: false, secrets_release: true });
    if (u.pathname === '/v1/events') return j({ items: [{ ts: new Date().toISOString(), kind: 'vm.started', vm: 'web-01', actor: 'admin' }, { ts: new Date(Date.now()-60000).toISOString(), kind: 'egress.changed', vm: 'web-01', actor: 'admin' }] });
    if (u.pathname === '/v1/openapi.json') return j({ paths: { '/v1/vms': { get: { summary: 'List machines' }, post: { summary: 'Create a machine' } }, '/v1/vms/{id}': { delete: { summary: 'Delete a machine' } }, '/v1/host/memory': { get: { summary: 'Host memory' } }, '/v1/pools': { get: { summary: 'List pools' }, post: { summary: 'Create a pool' } } } });
    if (req.method === 'POST' && u.pathname === '/v1/vms') { let b=''; req.on('data', d => b += d); req.on('end', () => { const r = JSON.parse(b); const vm = { id: 'vm-new', name: r.name, status: 'creating', backend: 'qemu', guest_ip: null, labels: {}, error: null, request: { vcpus: r.vcpus, memory_mib: r.memory_mib } }; vms.push(vm); j(vm); }); return; }
    if ((m = u.pathname.match(/^\/v1\/vms\/([^/]+)\/stats$/))) return j({ cpu_usage_percent: 8 + Math.random() * 40, memory_usage_bytes: (900 + Math.random() * 400) * 1048576, disk_read_bytes: 3e8, disk_write_bytes: 9e7 });
    if ((m = u.pathname.match(/^\/v1\/vms\/([^/]+)\/logs$/))) { res.writeHead(200, { 'content-type': 'text/plain' }); return res.end('[ 0.000000] Linux version 6.8.0\n[ 1.204] systemd[1]: Started Network Time Synchronization.\n[ 2.9] cloud-init[512]: Cloud-init v. 24.1 finished\nubuntu login: '); }
    if ((m = u.pathname.match(/^\/v1\/vms\/([^/]+)$/)) && req.method === 'GET') { const v = vms.find(x => x.id === m[1]); return v ? j(v) : j({ error: 'VM not found' }, 404); }
    if (u.pathname === '/v1/sandboxes/density') { res.writeHead(200, { 'content-type': 'application/json' }); return res.end('{"host_pressure_level":"low"}'); }
    if (u.pathname === '/v1/events/stream') {
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      const send = () => res.write(`event: vm.${['started','stopped','snapshot'][tick % 3]}\ndata: ${JSON.stringify({ vm_name: vms[tick++ % vms.length].name, message: 'ok' })}\n\n`);
      send(); const t = setInterval(send, 1800); req.on('close', () => clearInterval(t)); return;
    }
    res.writeHead(404, { 'content-type': 'application/json' }); return res.end('{"error":"missing"}');
  }
  if (u.pathname === '/console') { res.writeHead(308, { location: '/console/' }); return res.end(); }
  let p = u.pathname.replace(/^\/console\/?/, '') || 'index.html';
  let f = path.join(dist, p);
  if (!fs.existsSync(f) || fs.statSync(f).isDirectory()) f = path.join(dist, 'index.html');
  res.writeHead(200, { 'content-type': types[path.extname(f)] || 'application/octet-stream' });
  fs.createReadStream(f).pipe(res);
}).listen(Number(process.env.PORT || 7791), () => console.log('mock ready'));
