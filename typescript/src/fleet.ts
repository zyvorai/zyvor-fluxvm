// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

/**
 * Client for FluxVM central's fleet API (`/fleet/*`): the Mac cloud's tenants, API keys, leases, VMs, networks,
 * image catalog, webhooks, prices, billing, the activity log, volumes and warm pools. See
 * `schemas/fleet-openapi.yaml`.
 *
 * Idempotent calls (GET, PUT, DELETE) are retried on 429 and 5xx with exponential backoff, honouring Retry-After;
 * POSTs are never retried.
 */

import {
  ApiError,
  AuthError,
  Conflict,
  ConnectionFailed,
  ForbiddenError,
  NotFound,
  RateLimited,
  RequestTimeout,
  ServiceUnavailable,
} from "./errors.js";

type Json = Record<string, unknown>;

export interface FleetClientOptions {
  /** Central's URL, e.g. `https://fleet.example.com`. */
  baseUrl: string;
  /** The fleet (operator) token, a tenant's primary token, or a named API key. */
  token?: string;
  /** Per-attempt timeout in milliseconds (default 60 000). */
  timeoutMs?: number;
  /** Retries of an idempotent call after a 429, a 5xx or a connection failure (default 3). */
  retries?: number;
  /** First backoff delay in milliseconds (default 500); doubles each retry up to `retryMaxMs` (default 10 000). */
  retryDelayMs?: number;
  retryMaxMs?: number;
  /** Replace `fetch` (custom agent, tests). */
  fetch?: typeof fetch;
  /** Replace the backoff sleep (tests). */
  sleep?: (ms: number) => Promise<void>;
}

// ---- shapes (central's JSON; fields marked optional may be absent) -------------------------------------------

export interface Quota {
  max_vms?: number;
  max_vcpus?: number;
  max_memory_mib?: number;
  max_leased_hosts?: number;
}

export interface Tenant {
  id: string;
  display_name: string | null;
  quota: Quota;
  disabled: boolean;
  created_at: string;
  monthly_budget: number | null;
  in_use?: { vms: number; vcpus: number; memory_mib: number; leased_hosts: number };
  active_leases?: Lease[];
}

export interface ApiKey {
  id: string;
  name: string;
  created_at: string;
  expires_at: string | null;
  read_only: boolean;
  last_used_at: string | null;
}

export interface Node {
  name: string;
  fluxvm_url?: string;
  vcpus_total: number;
  memory_mib_total: number;
  vm_count: number;
  last_seen: string;
  healthy: boolean;
  cordoned?: boolean;
  labels?: Record<string, string>;
  free_vcpus: number;
  free_memory_mib: number;
  apple?: ({ nested_virtualization?: boolean } & Json) | null;
}

export interface Lease {
  id: string;
  tenant: string;
  node: string;
  started_at: string;
  min_end: string;
  ends_at?: string;
  released_at?: string;
  state: "active" | "released";
  rate_micros?: number;
  price_class?: string;
}

export interface CreateLeaseRequest {
  tenant?: string;
  node?: string;
  nodeSelector?: Record<string, string>;
  hours?: number;
  min_vcpus?: number;
  min_memory_mib?: number;
  nested_virtualization?: boolean;
  any_node?: boolean;
  allow_busy?: boolean;
}

export type VmStatus = "creating" | "running" | "paused" | "stopped" | "failed";

export interface Vm {
  id: string;
  name: string;
  node: string;
  backend: string;
  status: VmStatus;
  created_at: string;
  expires_at: string | null;
  error: string | null;
  request?: Json;
  [extra: string]: unknown;
}

export interface LaunchRequest {
  name: string;
  /** The disk's path on the Mac (see `launchFromCatalog`). */
  image: string;
  guestOs?: "macos" | "linux";
  vcpus?: number;
  memoryMib?: number;
  diskSizeGib?: number;
  ttlSeconds?: number;
  node?: string;
  nodeSelector?: Record<string, string>;
  /** Private networks (overlays) to join. */
  networks?: string[];
  /** Volumes to attach (requires central with volumes). */
  volumes?: string[];
  reschedulable?: boolean;
  /** Operator acting for a tenant. */
  tenant?: string;
  /** Further create-request fields; named fields win. */
  extra?: Json;
}

export type PowerAction = "start" | "stop" | "restart" | "pause" | "resume";

export interface ExecResult {
  exit_code: number;
  stdout: string;
  stderr: string;
  [extra: string]: unknown;
}

export interface Network {
  name: string;
  tenant: string | null;
  subnet: string;
  port: number;
  mtu: number;
  members: Record<string, [number, number]>;
  key_epoch: number;
  key_rotated_at: string;
  created_at: string;
}

export interface CatalogImage {
  name: string;
  display_name: string | null;
  description: string | null;
  guest_os: "macos" | "linux";
  image: string;
  vcpus: number;
  memory_mib: number;
  disk_size_gib: number | null;
  source: string | null;
  updated_at: string | null;
}

export interface Delivery {
  notification: string;
  type: string;
  time: string;
  attempt: number;
  status: number | null;
  ok: boolean;
  error?: string;
}

export interface Webhook {
  id: string;
  owner: string | null;
  url: string;
  events: string[];
  created_at: string;
  deliveries?: Delivery[];
}

export interface Notification {
  id: string;
  time: string;
  type: string;
  tenant?: string;
  title: string;
  detail?: string;
  link?: string;
}

export interface PriceList {
  currency: string;
  mac_classes: { name: string; min_vcpus: number; min_memory_gib: number; per_hour: number }[];
  mac_per_hour: number;
  vm_per_hour: number;
  vcpu_per_hour: number;
  memory_gib_per_hour: number;
  macos_vm_per_hour: number;
  charge_vms_on_leased_macs: boolean;
}

export interface Statement {
  tenant: string;
  currency: string;
  from: string;
  to: string;
  lines: { description: string; quantity: number; unit: string; unit_price: number; amount_micros: number; amount: number }[];
  total_micros: number;
  total: number;
  forecast?: number;
  budget?: { monthly: number; used: number; state: "ok" | "near" | "over" };
}

export interface Billing {
  from: string;
  to: string;
  currency: string;
  prices_set: boolean;
  statements: Statement[];
}

export interface AuditEvent {
  time: string;
  actor: string;
  tenant?: string;
  method: string;
  path: string;
  target?: string;
  status: number;
  key?: string;
}

export interface Volume {
  id: string;
  name: string;
  tenant: string | null;
  node: string;
  size_gib: number;
  attached_to: string | null;
  created_at: string;
}

/** A warm pool as central reports it: the size and the live counts are nested. */
export interface Pool {
  name: string;
  /** `operator` (a shared pool) or the tenant id. */
  owner?: string;
  tenant?: string | null;
  image: string;
  size: { vcpus: number; memory_mib: number };
  min_ready: number;
  max_total: number;
  node_selector?: Record<string, string>;
  nodes?: string[];
  status?: {
    state: "ready" | "filling" | "paused" | "backing_off";
    ready: number;
    booting: number;
    claimed_running: number;
    claimed_24h: number;
    avg_claim_ms?: number | null;
    last_error?: string | null;
    [extra: string]: unknown;
  };
  created_at?: string;
  [extra: string]: unknown;
}

/** `image`, `vcpus` and `memory_mib` are fixed once the pool exists (a PATCH may only repeat them). */
export interface PoolSpec {
  image: string;
  min_ready: number;
  vcpus?: number;
  memory_mib?: number;
  max_total?: number;
  nodes?: string[];
}

interface Answer {
  status: number;
  text: string;
  headers: Headers;
}

const IDEMPOTENT = new Set(["GET", "HEAD", "PUT", "DELETE"]);
const POWER: PowerAction[] = ["start", "stop", "restart", "pause", "resume"];
const enc = encodeURIComponent;

function query(params: Record<string, string | number | boolean | undefined | null>): string {
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) {
    if (v !== undefined && v !== null && v !== "") q.set(k, String(v));
  }
  const s = q.toString();
  return s ? `?${s}` : "";
}

function fleetError(status: number, text: string, headers: Headers): ApiError {
  let body: unknown = text;
  let message = text;
  try {
    const parsed: unknown = JSON.parse(text);
    body = parsed;
    if (parsed && typeof parsed === "object" && "error" in parsed) message = String((parsed as Json).error);
  } catch {
    // not JSON
  }
  switch (status) {
    case 401:
      return new AuthError(status, body, message);
    case 403:
      return new ForbiddenError(status, body, message);
    case 404:
      return new NotFound(status, body, message);
    case 409:
      return new Conflict(status, body, message);
    case 429: {
      const n = Number(headers.get("retry-after") ?? NaN);
      return new RateLimited(status, body, message, Number.isFinite(n) ? n : undefined);
    }
    case 503:
      return new ServiceUnavailable(status, body, message);
  }
  return new ApiError(status, body, message);
}

/** Client for one central (`fluxvm-agent central`). */
export class FleetClient {
  static readonly ME = "me";
  readonly baseUrl: string;
  readonly token?: string;
  private readonly timeoutMs: number;
  private readonly retries: number;
  private readonly retryDelayMs: number;
  private readonly retryMaxMs: number;
  private readonly fetchFn: typeof fetch;
  private readonly sleep: (ms: number) => Promise<void>;

  constructor(opts: FleetClientOptions) {
    if (!opts.baseUrl) throw new Error("baseUrl is required");
    this.baseUrl = opts.baseUrl.replace(/\/+$/, "");
    this.token = opts.token;
    this.timeoutMs = opts.timeoutMs ?? 60_000;
    this.retries = Math.max(0, opts.retries ?? 3);
    this.retryDelayMs = opts.retryDelayMs ?? 500;
    this.retryMaxMs = opts.retryMaxMs ?? 10_000;
    this.fetchFn = opts.fetch ?? fetch;
    this.sleep = opts.sleep ?? ((ms) => new Promise((r) => setTimeout(r, ms)));
  }

  private async once(method: string, path: string, body: string | undefined, accept: string): Promise<Answer> {
    const headers: Record<string, string> = { Accept: accept, "User-Agent": "fluxvm-fleet-ts/0.1.0" };
    if (body !== undefined) headers["Content-Type"] = "application/json";
    if (this.token) headers["Authorization"] = `Bearer ${this.token}`;
    const ctl = new AbortController();
    const timer = setTimeout(() => ctl.abort(), this.timeoutMs);
    try {
      const resp = await this.fetchFn(this.baseUrl + path, { method, headers, body, redirect: "manual", signal: ctl.signal });
      // The body is read inside the timeout too.
      return { status: resp.status, text: await resp.text(), headers: resp.headers };
    } catch (err) {
      if (ctl.signal.aborted) throw new RequestTimeout(`${method} ${path} timed out`);
      throw new ConnectionFailed(`${method} ${this.baseUrl}${path}: ${(err as Error).message}`);
    } finally {
      clearTimeout(timer);
    }
  }

  private backoff(attempt: number, retryAfterSecs?: number): number {
    let ms = Math.min(this.retryDelayMs * 2 ** (attempt - 1), this.retryMaxMs);
    ms += Math.random() * (ms / 4);
    if (retryAfterSecs !== undefined && retryAfterSecs * 1000 > ms) ms = Math.min(retryAfterSecs * 1000, this.retryMaxMs);
    return ms;
  }

  private async raw(method: string, path: string, payload?: unknown, accept = "application/json"): Promise<Answer> {
    const body = payload === undefined ? undefined : JSON.stringify(payload);
    const attempts = 1 + (IDEMPOTENT.has(method) ? this.retries : 0);
    let last: unknown;
    let retryAfter: number | undefined;
    for (let attempt = 0; attempt < attempts; attempt++) {
      if (attempt > 0) await this.sleep(this.backoff(attempt, retryAfter));
      let resp: Answer;
      try {
        resp = await this.once(method, path, body, accept);
      } catch (err) {
        last = err;
        retryAfter = undefined;
        if (attempt + 1 < attempts && (err instanceof ConnectionFailed || err instanceof RequestTimeout)) continue;
        throw err;
      }
      if ((resp.status === 429 || resp.status >= 500) && attempt + 1 < attempts) {
        const n = Number(resp.headers.get("retry-after") ?? NaN);
        retryAfter = Number.isFinite(n) ? n : undefined;
        continue;
      }
      return resp;
    }
    throw last;
  }

  private async json<T>(method: string, path: string, payload?: unknown): Promise<T> {
    const { status, text, headers } = await this.raw(method, path, payload);
    if (status < 200 || status > 299) throw fleetError(status, text, headers);
    if (!text) return undefined as T;
    try {
      return JSON.parse(text) as T;
    } catch {
      throw new ApiError(status, text, "response was not JSON");
    }
  }

  private async text(path: string, accept: string): Promise<string> {
    const { status, text, headers } = await this.raw("GET", path, undefined, accept);
    if (status < 200 || status > 299) throw fleetError(status, text, headers);
    return text;
  }

  /** Any fleet route this client does not wrap yet. */
  request<T = unknown>(method: string, path: string, payload?: unknown): Promise<T> {
    return this.json<T>(method.toUpperCase(), path, payload);
  }

  async health(): Promise<boolean> {
    return Boolean((await this.json<{ ok?: boolean }>("GET", "/healthz"))?.ok);
  }

  // ---- tenants (operator) and the calling tenant -------------------------------------------------------------

  async listTenants(): Promise<Tenant[]> {
    return (await this.json<{ items: Tenant[] }>("GET", "/fleet/tenants")).items;
  }

  /** The token in the answer is shown once. */
  createTenant(id: string, opts: { displayName?: string; quota?: Quota } = {}): Promise<{ tenant: Tenant; token: string; note: string }> {
    const body: Json = { id, quota: opts.quota ?? {} };
    if (opts.displayName !== undefined) body.display_name = opts.displayName;
    return this.json("POST", "/fleet/tenants", body);
  }

  getTenant(id: string): Promise<Tenant> {
    return this.json("GET", `/fleet/tenants/${enc(id)}`);
  }

  me(): Promise<Tenant> {
    return this.json("GET", "/fleet/tenants/me");
  }

  updateTenant(id: string, patch: { quota?: Quota; disabled?: boolean; display_name?: string }): Promise<Tenant> {
    return this.json("PATCH", `/fleet/tenants/${enc(id)}`, patch);
  }

  async deleteTenant(id: string): Promise<void> {
    await this.json("DELETE", `/fleet/tenants/${enc(id)}`);
  }

  rotateTenantToken(id: string): Promise<{ tenant: string; token: string }> {
    return this.json("POST", `/fleet/tenants/${enc(id)}/rotate-token`);
  }

  /** `null` (or 0) removes the budget. */
  setBudget(monthly: number | null, tenant = FleetClient.ME): Promise<Tenant> {
    return this.json("PUT", `/fleet/tenants/${enc(tenant)}/budget`, { monthly });
  }

  // ---- API keys ------------------------------------------------------------------------------------------------

  async listKeys(tenant = FleetClient.ME): Promise<ApiKey[]> {
    return (await this.json<{ items: ApiKey[] }>("GET", `/fleet/tenants/${enc(tenant)}/keys`)).items;
  }

  /** With the tenant's primary token (a key cannot make keys) or the operator. The token is shown once. */
  createKey(name: string, opts: { readOnly?: boolean; expiresInDays?: number; tenant?: string } = {}): Promise<{ key: ApiKey; token: string; note: string }> {
    const body: Json = { name, read_only: opts.readOnly ?? false };
    if (opts.expiresInDays !== undefined) body.expires_in_days = opts.expiresInDays;
    return this.json("POST", `/fleet/tenants/${enc(opts.tenant ?? FleetClient.ME)}/keys`, body);
  }

  async revokeKey(idOrName: string, tenant = FleetClient.ME): Promise<ApiKey> {
    return (await this.json<{ revoked: ApiKey }>("DELETE", `/fleet/tenants/${enc(tenant)}/keys/${enc(idOrName)}`)).revoked;
  }

  // ---- nodes -------------------------------------------------------------------------------------------------

  async listNodes(): Promise<Node[]> {
    return (await this.json<{ items: Node[] }>("GET", "/fleet/nodes")).items;
  }

  /** A tenant sees only a Mac it leases. */
  getNode(name: string): Promise<Node> {
    return this.json("GET", `/fleet/nodes/${enc(name)}`);
  }

  capacity(): Promise<Json> {
    return this.json("GET", "/fleet/capacity");
  }

  // ---- leases --------------------------------------------------------------------------------------------------

  /** A whole Mac for at least 24 hours. */
  createLease(req: CreateLeaseRequest = {}): Promise<{ lease: Lease; node: Node }> {
    return this.json("POST", "/fleet/leases", req);
  }

  async listLeases(opts: { state?: "active" | "released"; tenant?: string } = {}): Promise<Lease[]> {
    return (await this.json<{ items: Lease[] }>("GET", `/fleet/leases${query(opts)}`)).items;
  }

  getLease(id: string): Promise<Lease> {
    return this.json("GET", `/fleet/leases/${enc(id)}`);
  }

  /** Refused (`Conflict`) before the 24-hour minimum unless the operator forces it. */
  releaseLease(id: string, force = false): Promise<{ lease: Lease; evicted: Json[]; scrub_required: boolean }> {
    return this.json("POST", `/fleet/leases/${enc(id)}/release${force ? "?force=true" : ""}`);
  }

  // ---- VMs -----------------------------------------------------------------------------------------------------

  launchVm(req: LaunchRequest): Promise<{ node: string; vm: Vm }> {
    const body: Json = { ...(req.extra ?? {}) };
    const apple: Json = { ...((body.apple as Json | undefined) ?? {}), guest_os: req.guestOs ?? "macos" };
    if (req.networks?.length) apple.networks = req.networks.map((name) => ({ name }));
    Object.assign(body, { name: req.name, backend: (body.backend as string | undefined) ?? "vz", image: req.image, apple });
    const opt: [string, unknown][] = [
      ["vcpus", req.vcpus],
      ["memory_mib", req.memoryMib],
      ["disk_size_gib", req.diskSizeGib],
      ["ttl_seconds", req.ttlSeconds],
      ["node", req.node],
      ["tenant", req.tenant],
    ];
    for (const [k, v] of opt) if (v !== undefined) body[k] = v;
    if (req.nodeSelector && Object.keys(req.nodeSelector).length) body.nodeSelector = req.nodeSelector;
    if (req.volumes?.length) body.volumes = req.volumes;
    if (req.reschedulable) body.reschedulable = true;
    return this.json("POST", "/fleet/vms", body);
  }

  /** Launch from an image catalog entry, with its size and guest OS as defaults. */
  async launchFromCatalog(name: string, image: string, opts: Omit<LaunchRequest, "name" | "image"> = {}): Promise<{ node: string; vm: Vm }> {
    const entry = await this.getImage(image);
    return this.launchVm({
      ...opts,
      name,
      image: entry.image,
      guestOs: opts.guestOs ?? entry.guest_os,
      vcpus: opts.vcpus ?? entry.vcpus,
      memoryMib: opts.memoryMib ?? entry.memory_mib,
      diskSizeGib: opts.diskSizeGib ?? entry.disk_size_gib ?? undefined,
    });
  }

  listVms(): Promise<{ items: Vm[]; unreachable_nodes: { node: string; reason: string }[] }> {
    return this.json("GET", "/fleet/vms");
  }

  /** One VM from the fleet list (there is no single-VM route). `NotFound` when not listed; `ServiceUnavailable` when its Mac did not answer. */
  async getVm(node: string, id: string): Promise<Vm> {
    const list = await this.listVms();
    const vm = list.items.find((v) => v.node === node && v.id === id);
    if (vm) return vm;
    const down = list.unreachable_nodes.find((u) => u.node === node);
    if (down) throw new ServiceUnavailable(503, down, `node '${node}' did not answer: ${down.reason}`);
    throw new NotFound(404, null, `no VM '${id}' on node '${node}'`);
  }

  async findVm(name: string): Promise<Vm> {
    const matches = (await this.listVms()).items.filter((v) => v.name === name);
    if (matches.length === 0) throw new NotFound(404, null, `no VM named '${name}'`);
    if (matches.length > 1) throw new Conflict(409, null, `more than one VM is named '${name}'`);
    return matches[0];
  }

  async deleteVm(node: string, id: string): Promise<void> {
    await this.json("DELETE", `/fleet/vms/${enc(node)}/${enc(id)}`);
  }

  powerVm(node: string, id: string, action: PowerAction): Promise<Vm> {
    if (!POWER.includes(action)) throw new Error(`action must be one of ${POWER.join(", ")}`);
    return this.json("POST", `/fleet/vms/${enc(node)}/${enc(id)}/${action}`);
  }

  execVm(node: string, id: string, command: string, timeoutSeconds?: number): Promise<ExecResult> {
    const body: Json = { command };
    if (timeoutSeconds !== undefined) body.timeout_seconds = timeoutSeconds;
    return this.json("POST", `/fleet/vms/${enc(node)}/${enc(id)}/exec`, body);
  }

  vmLogs(node: string, id: string, lines?: number): Promise<string> {
    return this.text(`/fleet/vms/${enc(node)}/${enc(id)}/logs${query({ lines })}`, "text/plain");
  }

  // ---- networks ------------------------------------------------------------------------------------------------

  async listNetworks(): Promise<Network[]> {
    return (await this.json<{ items: Network[] }>("GET", "/fleet/overlays")).items;
  }

  createNetwork(name: string, opts: { nodes?: string[]; hosts_per_node?: number; mtu?: number; tenant?: string } = {}): Promise<Network> {
    return this.json("POST", "/fleet/overlays", { name, ...opts });
  }

  getNetwork(name: string): Promise<Network> {
    return this.json("GET", `/fleet/overlays/${enc(name)}`);
  }

  async deleteNetwork(name: string): Promise<void> {
    await this.json("DELETE", `/fleet/overlays/${enc(name)}`);
  }

  addNetworkMember(name: string, node: string): Promise<Network> {
    return this.json("POST", `/fleet/overlays/${enc(name)}/members`, { node });
  }

  removeNetworkMember(name: string, node: string): Promise<Network> {
    return this.json("DELETE", `/fleet/overlays/${enc(name)}/members/${enc(node)}`);
  }

  rotateNetworkKey(name: string): Promise<Network> {
    return this.json("POST", `/fleet/overlays/${enc(name)}/rotate`);
  }

  // ---- image catalog -------------------------------------------------------------------------------------------

  async listImages(): Promise<CatalogImage[]> {
    return (await this.json<{ items: CatalogImage[] }>("GET", "/fleet/images")).items;
  }

  async getImage(name: string): Promise<CatalogImage> {
    const found = (await this.listImages()).find((i) => i.name === name);
    if (!found) throw new NotFound(404, null, `no image '${name}' in the catalog`);
    return found;
  }

  /** Operator. */
  putImage(name: string, entry: Omit<Partial<CatalogImage>, "name"> & { image: string; guest_os: "macos" | "linux" }): Promise<CatalogImage> {
    return this.json("PUT", `/fleet/images/${enc(name)}`, entry);
  }

  async deleteImage(name: string): Promise<void> {
    await this.json("DELETE", `/fleet/images/${enc(name)}`);
  }

  // ---- webhooks and notifications --------------------------------------------------------------------------------

  listWebhooks(): Promise<{ items: Webhook[]; kinds: string[] }> {
    return this.json("GET", "/fleet/webhooks");
  }

  /** The secret in the answer is shown once. */
  createWebhook(url: string, events?: string[]): Promise<{ webhook: Webhook; secret: string; note: string }> {
    return this.json("POST", "/fleet/webhooks", events?.length ? { url, events } : { url });
  }

  async deleteWebhook(id: string): Promise<void> {
    await this.json("DELETE", `/fleet/webhooks/${enc(id)}`);
  }

  async testWebhook(id: string): Promise<Delivery> {
    return (await this.json<{ delivery: Delivery }>("POST", `/fleet/webhooks/${enc(id)}/test`)).delivery;
  }

  async notifications(limit?: number): Promise<Notification[]> {
    return (await this.json<{ items: Notification[] }>("GET", `/fleet/notifications${query({ limit })}`)).items;
  }

  // ---- prices, billing, usage, activity --------------------------------------------------------------------------

  prices(): Promise<PriceList> {
    return this.json("GET", "/fleet/prices");
  }

  /** Operator. */
  setPrices(prices: Partial<PriceList>): Promise<PriceList> {
    return this.json("PUT", "/fleet/prices", prices);
  }

  billing(opts: { month?: string; tenant?: string } = {}): Promise<Billing> {
    return this.json("GET", `/fleet/billing${query(opts)}`);
  }

  billingCsv(opts: { month?: string; tenant?: string } = {}): Promise<string> {
    return this.text(`/fleet/billing${query({ ...opts, format: "csv" })}`, "text/csv");
  }

  usage(opts: { from?: string; to?: string; tenant?: string } = {}): Promise<Json> {
    return this.json("GET", `/fleet/usage${query(opts)}`);
  }

  async events(opts: { limit?: number; tenant?: string } = {}): Promise<AuditEvent[]> {
    return (await this.json<{ items: AuditEvent[] }>("GET", `/fleet/events${query(opts)}`)).items;
  }

  // ---- volumes (requires central with volumes) -------------------------------------------------------------------

  async listVolumes(): Promise<Volume[]> {
    return (await this.json<{ items: Volume[] }>("GET", "/fleet/volumes")).items;
  }

  createVolume(name: string, sizeGib: number, node?: string): Promise<Volume> {
    return this.json("POST", "/fleet/volumes", node === undefined ? { name, size_gib: sizeGib } : { name, size_gib: sizeGib, node });
  }

  getVolume(name: string): Promise<Volume> {
    return this.json("GET", `/fleet/volumes/${enc(name)}`);
  }

  resizeVolume(name: string, sizeGib: number): Promise<Volume> {
    return this.json("POST", `/fleet/volumes/${enc(name)}/resize`, { size_gib: sizeGib });
  }

  async deleteVolume(name: string): Promise<void> {
    await this.json("DELETE", `/fleet/volumes/${enc(name)}`);
  }

  // ---- warm pools (requires central with pools) ------------------------------------------------------------------

  async listPools(): Promise<Pool[]> {
    return (await this.json<{ items: Pool[] }>("GET", "/fleet/pools")).items;
  }

  createPool(name: string, spec: PoolSpec): Promise<Pool> {
    return this.json("POST", "/fleet/pools", { name, ...spec });
  }

  getPool(name: string): Promise<Pool> {
    return this.json("GET", `/fleet/pools/${enc(name)}`);
  }

  updatePool(name: string, patch: Partial<PoolSpec>): Promise<Pool> {
    return this.json("PATCH", `/fleet/pools/${enc(name)}`, patch);
  }

  async deletePool(name: string): Promise<void> {
    await this.json("DELETE", `/fleet/pools/${enc(name)}`);
  }
}
