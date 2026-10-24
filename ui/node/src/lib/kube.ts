// Pure helpers for the Kubernetes page: how the daemon's /v1/kube answers are summarised and filtered.

export type Tone = 'ok' | 'warn' | 'bad' | 'info' | 'idle';

export type KubePod = {
  namespace: string;
  name: string;
  status: string;
  phase: string | null;
  ready: string;
  restarts: number;
  node: string | null;
  pod_ip: string | null;
  created: string | null;
};

export type KubeNode = {
  name: string;
  ready: boolean;
  roles: string[];
  version: string | null;
  internal_ip: string | null;
  os: string | null;
  cpu: string | null;
  memory_mib: number | null;
  created: string | null;
};

export type KubeClusterItem = { name: string; nodes: { name: string; status: string; role: 'server' | 'worker' }[] };

export type KubeDetail = {
  cluster: string;
  api: string;
  vms: { name: string; status: string }[];
  nodes: KubeNode[];
  pods: KubePod[];
  truncated: boolean;
};

const BAD = /^(CrashLoopBackOff|Error|ErrImagePull|ImagePullBackOff|InvalidImageName|OOMKilled|Evicted|Failed|CreateContainerError|CreateContainerConfigError|RunContainerError|DeadlineExceeded|Unknown)$/;
const WAIT = /^(Pending|ContainerCreating|PodInitializing|Terminating|Init:.*)$/;

/** The colour of a pod: a Running pod that is not fully ready is still on its way. */
export const podTone = (p: Pick<KubePod, 'status' | 'ready'>): Tone => {
  if (BAD.test(p.status)) return 'bad';
  if (WAIT.test(p.status)) return 'warn';
  if (p.status === 'Running') {
    const [have, want] = p.ready.split('/').map(Number);
    return have === want ? 'ok' : 'warn';
  }
  if (p.status === 'Succeeded' || p.status === 'Completed') return 'idle';
  return 'idle';
};

export type PodSummary = { total: number; healthy: number; waiting: number; failing: number; done: number };

export const summarize = (pods: KubePod[]): PodSummary => {
  const s: PodSummary = { total: pods.length, healthy: 0, waiting: 0, failing: 0, done: 0 };
  for (const p of pods) {
    const t = podTone(p);
    if (t === 'ok') s.healthy++;
    else if (t === 'warn') s.waiting++;
    else if (t === 'bad') s.failing++;
    else s.done++;
  }
  return s;
};

export const namespacesOf = (pods: KubePod[]): string[] => [...new Set(pods.map((p) => p.namespace))].sort();

/** Namespace (empty = all) and a free-text needle over name, node and IP. Failing pods sort first, then by name. */
export const filterPods = (pods: KubePod[], namespace: string, needle: string): KubePod[] => {
  const q = needle.trim().toLowerCase();
  const rank = (p: KubePod) => ({ bad: 0, warn: 1, ok: 2, info: 3, idle: 3 })[podTone(p)];
  return pods
    .filter((p) => !namespace || p.namespace === namespace)
    .filter((p) => !q || `${p.name} ${p.node ?? ''} ${p.pod_ip ?? ''} ${p.status}`.toLowerCase().includes(q))
    .sort((a, b) => rank(a) - rank(b) || a.namespace.localeCompare(b.namespace) || a.name.localeCompare(b.name));
};

/** `45s`, `12m`, `3h`, `2d`: the first unit that is not zero, like kubectl. */
export const ageOf = (created: string | null, now: number): string => {
  const t = created ? Date.parse(created) : NaN;
  if (Number.isNaN(t)) return '—';
  const s = Math.max(0, Math.floor((now - t) / 1000));
  if (s < 90) return `${s}s`;
  if (s < 90 * 60) return `${Math.floor(s / 60)}m`;
  if (s < 48 * 3600) return `${Math.floor(s / 3600)}h`;
  return `${Math.floor(s / 86400)}d`;
};

/** What to tell someone who sees no pods, by the status the daemon answered with. */
export const kubeProblem = (message: string): { title: string; body: string } => {
  if (/no kubeconfig/i.test(message)) return { title: 'The daemon cannot read this cluster’s kubeconfig', body: message };
  if (/is (stopped|paused|failed|creating)/i.test(message)) return { title: 'The cluster’s server machine is not running', body: `${message}. Start it from Machines, or run fluxctl kube up.` };
  if (/no address/i.test(message)) return { title: 'The server machine has no address yet', body: 'It is still starting. This page refreshes on its own.' };
  return { title: 'Could not read the cluster', body: message };
};
