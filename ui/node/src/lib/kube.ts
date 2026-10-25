// Pure helpers for the Kubernetes page: how the daemon's /v1/kube answers are summarised and filtered.

export type Tone = 'ok' | 'warn' | 'bad' | 'info' | 'idle';

export type KubePod = {
  namespace: string;
  name: string;
  containers?: string[];
  init_containers?: string[];
  status: string;
  phase: string | null;
  ready: string;
  restarts: number;
  node: string | null;
  pod_ip: string | null;
  created: string | null;
  /** From metrics-server; absent when it does not answer. */
  cpu_m?: number;
  mem_mib?: number;
};

export type KubeNode = {
  name: string;
  ready: boolean;
  roles: string[];
  version: string | null;
  internal_ip: string | null;
  /** Cordoned: no new pods are placed on it. */
  unschedulable?: boolean;
  os: string | null;
  cpu: string | null;
  memory_mib: number | null;
  created: string | null;
  cpu_m?: number;
  mem_used_mib?: number;
  cpu_percent?: number | null;
  mem_percent?: number | null;
};

export type KubeClusterItem = { name: string; nodes: { name: string; status: string; role: 'server' | 'worker' }[] };

export type KubeDetail = {
  cluster: string;
  api: string;
  vms: { name: string; status: string }[];
  nodes: KubeNode[];
  pods: KubePod[];
  truncated: boolean;
  /** True when metrics-server gave CPU and memory numbers. */
  metrics?: boolean;
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

/** CPU as `kubectl top` shows it: millicores, or cores from 1000m. */
export const fmtCpu = (m: number | undefined): string => (m == null ? '—' : m < 1000 ? `${m}m` : `${(m / 1000).toFixed(m < 10000 ? 2 : 1)}`);

export const fmtMem = (mib: number | undefined): string => (mib == null ? '—' : mib >= 1024 ? `${(mib / 1024).toFixed(1)} GiB` : `${mib} MiB`);

/** How full something is: red from 90 %, amber from 75 %. */
export const usageTone = (percent: number | null | undefined): Tone => (percent == null ? 'idle' : percent >= 90 ? 'bad' : percent >= 75 ? 'warn' : 'ok');

// ---- workloads -------------------------------------------------------------------------------------------------------------------

export type Replicated = { namespace: string; name: string; desired: number; ready: number; updated: number; available: number; images: string[]; created: string | null };

export type ServicePort = { name: string | null; protocol: string; port: number; target_port: number | string | null; node_port: number | null };

export type KubeService = { namespace: string; name: string; type: string; cluster_ip: string | null; external: string[]; ports: ServicePort[]; selector: string[]; created: string | null };

export type Workloads = { cluster: string; deployments: Replicated[]; statefulsets: Replicated[]; daemonsets: Replicated[]; services: KubeService[] };

/** Ready of wanted: green when all are up, red when none are, amber between; grey for something scaled to zero. */
export const readyTone = (w: Pick<Replicated, 'ready' | 'desired'>): Tone => (w.desired === 0 ? 'idle' : w.ready >= w.desired ? 'ok' : w.ready === 0 ? 'bad' : 'warn');

/** `80→8080/TCP`, with `:30080` for a NodePort. */
export const portText = (p: ServicePort): string => `${p.port}${p.target_port != null && String(p.target_port) !== String(p.port) ? `→${p.target_port}` : ''}/${p.protocol}${p.node_port != null ? `:${p.node_port}` : ''}`;

/** `nginx:1.29-alpine` from `docker.io/library/nginx:1.29-alpine`, and `app@sha256:…` kept short. */
export const shortImage = (image: string): string => {
  const [repo, digest] = image.split('@');
  const name = repo.split('/').slice(-1)[0];
  return digest ? `${name}@${digest.slice(0, 15)}` : name;
};

export const inNamespace = <T extends { namespace: string }>(items: T[], ns: string): T[] => (ns ? items.filter((i) => i.namespace === ns) : items);

export const namespacesOfAll = (w: Workloads): string[] => [...new Set([...w.deployments, ...w.statefulsets, ...w.daemonsets, ...w.services].map((i) => i.namespace))].sort();

// ---- events ----------------------------------------------------------------------------------------------------------------------

export type KubeEvent = { namespace: string | null; type: string; reason: string | null; message: string | null; object: string; kind: string; name: string; count: number; first: string | null; last: string | null; component: string | null };

export type EventFilter = { warningsOnly: boolean; namespace: string; text: string };

export const eventTone = (type: string): Tone => (type === 'Warning' ? 'warn' : 'idle');

/** Keeps the order the daemon sent (newest first). The text matches reason, object, message and component. */
export const filterEvents = (events: KubeEvent[], f: EventFilter): KubeEvent[] => {
  const q = f.text.trim().toLowerCase();
  return events.filter((e) => (!f.warningsOnly || e.type === 'Warning') && (!f.namespace || e.namespace === f.namespace) && (!q || `${e.reason ?? ''} ${e.object} ${e.message ?? ''} ${e.component ?? ''}`.toLowerCase().includes(q)));
};

export const namespacesOfEvents = (events: KubeEvent[]): string[] => [...new Set(events.map((e) => e.namespace).filter((n): n is string => !!n))].sort();

/** The workload kinds the actions accept, as the API spells them in a path. */
export type WorkloadKind = 'deployments' | 'statefulsets' | 'daemonsets';

/** Scaling goes from 0 to 100 replicas, and a DaemonSet has none to set. */
export const MAX_REPLICAS = 100;
export const canScale = (kind: WorkloadKind): boolean => kind !== 'daemonsets';
export const clampReplicas = (n: number): number => (Number.isFinite(n) ? Math.min(MAX_REPLICAS, Math.max(0, Math.round(n))) : 0);

export type KubeVerb = 'deleted' | 'scaled' | 'restarted' | 'cordoned' | 'uncordoned';

/** The toast for an action the daemon accepted: `web scaled to 3`, `web-7d restarted`. */
export const actionToast = (subject: string, verb: KubeVerb, replicas?: number): string => (verb === 'scaled' && replicas != null ? `${subject} scaled to ${replicas}` : `${subject} ${verb}`);

/** The daemon path of an action on a workload, a pod or a node; every part is escaped. */
export const actionPath = (
  cluster: string,
  what: { pod: { ns: string; name: string } } | { workload: { ns: string; kind: WorkloadKind; name: string }; action: 'scale' | 'restart' } | { node: string; action: 'cordon' | 'uncordon' },
): string => {
  const e = encodeURIComponent;
  const base = `/v1/kube/${e(cluster)}`;
  if ('pod' in what) return `${base}/pods/${e(what.pod.ns)}/${e(what.pod.name)}/delete`;
  if ('workload' in what) return `${base}/workloads/${e(what.workload.ns)}/${what.workload.kind}/${e(what.workload.name)}/${what.action}`;
  return `${base}/nodes/${e(what.node)}/${what.action}`;
};
