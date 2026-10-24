// How to reach a machine: the commands that work for it, built from what the daemon knows. Pure, so it can be tested.

export interface Forward {
  host_port: number;
  guest_port: number;
  guests?: boolean;
}

export interface ConnectVm {
  id: string;
  name: string;
  status: string;
  backend: string;
  guest_ip?: string | null;
  request?: { network?: unknown; cloud_init?: { user?: string } };
}

export interface Route {
  key: string;
  title: string;
  /** What to type, or '' when this route is not possible now. */
  command: string;
  note: string;
  /** False when something is missing, with `note` saying what. */
  ready: boolean;
}

/** A word safe in a shell as is; anything else is single-quoted. */
export const shq = (s: string): string => (/^[A-Za-z0-9_.@:/=+-]+$/.test(s) ? s : `'${s.replace(/'/g, `'\\''`)}'`);

export const forwardsOf = (vm: ConnectVm): Forward[] => {
  const f = (vm.request?.network as { forwards?: Forward[] } | undefined)?.forwards;
  return Array.isArray(f) ? f.filter((x) => Number.isInteger(x?.host_port) && Number.isInteger(x?.guest_port)) : [];
};

/** The login the CLI would use: the user cloud-init created, else root. */
export const loginOf = (vm: ConnectVm): string => vm.request?.cloud_init?.user?.trim() || 'root';

const isIp = (s?: string | null): s is string => !!s && /^[0-9a-fA-F:.]+$/.test(s);

/** The ways in, best first. `host` is where this browser reached the daemon (the machine running FluxVM). */
export function connectRoutes(vm: ConnectVm, host: string): Route[] {
  const running = vm.status === 'running';
  const user = loginOf(vm);
  const sshd = 'The guest needs sshd and a key from its cloud-init (or a password).';
  const out: Route[] = [];
  const ip = isIp(vm.guest_ip) ? vm.guest_ip : '';
  out.push({
    key: 'cli',
    title: 'From the machine running FluxVM',
    command: running && (ip || forwardsOf(vm).some((f) => f.guest_port === 22)) ? `fluxctl ssh ${shq(vm.id)}${user === 'root' ? '' : ` -l ${shq(user)}`}` : '',
    ready: running && !!(ip || forwardsOf(vm).some((f) => f.guest_port === 22)),
    note: !running ? 'Start the machine first.' : ip ? `Uses the machine's address (${ip}). ${sshd}` : forwardsOf(vm).some((f) => f.guest_port === 22) ? `Uses its published SSH port on this machine. ${sshd}` : 'The machine has no address yet; it is still starting.',
  });
  out.push({
    key: 'ssh',
    title: 'Plain OpenSSH to its address',
    command: running && ip ? `ssh ${shq(user)}@${ip}` : '',
    ready: running && !!ip,
    note: !running ? 'Start the machine first.' : ip ? `Works from the machine that runs FluxVM, which can route to ${ip}. Other computers usually cannot reach a private address like that one.` : 'No address yet.',
  });
  for (const f of forwardsOf(vm).filter((x) => x.guest_port === 22)) {
    out.push({
      key: `fwd-${f.host_port}`,
      title: `Through the published port ${f.host_port}`,
      command: running ? `ssh -p ${f.host_port} ${shq(user)}@${shq(host)}` : '',
      ready: running,
      note: running ? `Port ${f.host_port} on ${host} forwards to the machine's port 22${f.guests ? ' (also for other guests)' : ''}. Reachable from wherever that port is reachable, so only expose it on a network you trust, and use keys, not passwords.` : 'Start the machine first.',
    });
  }
  out.push({
    key: 'fleet',
    title: 'From anywhere, through your fleet’s central',
    command: `fluxctl fleet ssh ${shq(vm.name)}${user === 'root' ? '' : ` -l ${shq(user)}`}`,
    ready: true,
    note: 'Central checks who you are, allows only the ports its policy lists and closes idle tunnels; no port on this Mac is opened. Needs this Mac enrolled in a fleet.',
  });
  return out;
}

/** Whether this machine has a screen to show: a macOS-style `vz` guest that is running. */
export const hasDisplay = (vm: Pick<ConnectVm, 'backend' | 'status'>): boolean => vm.backend === 'vz' && vm.status === 'running';

/** `http://host:port` for a published port that is plausibly a web server, so the table can offer a link. */
export const WEB_PORTS = [80, 443, 3000, 5000, 8000, 8080, 8443, 8888, 9000];
export const webUrl = (f: Forward, host: string): string | null =>
  WEB_PORTS.includes(f.guest_port) ? `${f.guest_port === 443 || f.guest_port === 8443 ? 'https' : 'http'}://${host}:${f.host_port}/` : null;
