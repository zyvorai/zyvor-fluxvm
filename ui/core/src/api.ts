// Typed fetch wrapper for the FluxVM REST API. The UI is served by the daemon itself (same origin), so there
// is no CORS; the bearer lives in sessionStorage (never in a URL) exactly like the legacy console.
const TOKEN_KEY = 'fluxvm.token';

export class ApiError extends Error {
  constructor(
    public status: number,
    public code: string,
    message: string,
  ) {
    super(message);
  }
}
export class Unauthorized extends ApiError {
  constructor(message = 'Sign in required') {
    super(401, 'UNAUTHORIZED', message);
  }
}

export const getToken = (): string => {
  try {
    return sessionStorage.getItem(TOKEN_KEY) || '';
  } catch {
    return '';
  }
};
export const setToken = (t: string): void => {
  try {
    if (t) sessionStorage.setItem(TOKEN_KEY, t);
    else sessionStorage.removeItem(TOKEN_KEY);
  } catch {
    /* ignore */
  }
};

/** `/console/...` → ''; behind a reverse proxy at `/flux/console/...` → '/flux'. */
export const apiBase = (): string => location.pathname.replace(/\/console(\/.*)?$/, '');

type Listener = () => void;
const authListeners = new Set<Listener>();
export const onUnauthorized = (fn: Listener): (() => void) => {
  authListeners.add(fn);
  return () => authListeners.delete(fn);
};

export interface ReqOpts {
  body?: unknown;
  text?: boolean;
  signal?: AbortSignal;
  headers?: Record<string, string>;
}

export async function request<T = unknown>(method: string, path: string, opts: ReqOpts = {}): Promise<T> {
  const headers: Record<string, string> = { Accept: 'application/json', ...opts.headers };
  const token = getToken();
  if (token) headers.Authorization = `Bearer ${token}`;
  let body: BodyInit | undefined;
  if (opts.body !== undefined) {
    headers['Content-Type'] = 'application/json';
    body = JSON.stringify(opts.body);
  }
  const res = await fetch(apiBase() + path, { method, headers, body, signal: opts.signal, cache: 'no-store' });
  if (res.status === 401) {
    authListeners.forEach((f) => f());
    throw new Unauthorized();
  }
  if (!res.ok) {
    let code = String(res.status);
    let msg = res.statusText || `HTTP ${res.status}`;
    try {
      const j = await res.json();
      if (j?.error) msg = j.error;
      if (j?.code) code = j.code;
    } catch {
      /* non-JSON error body */
    }
    throw new ApiError(res.status, code, msg);
  }
  if (res.status === 204) return undefined as T;
  if (opts.text) return (await res.text()) as T;
  const ct = res.headers.get('content-type') || '';
  return (ct.includes('json') ? await res.json() : await res.text()) as T;
}

export const api = {
  get: <T = unknown>(p: string, o?: ReqOpts) => request<T>('GET', p, o),
  post: <T = unknown>(p: string, body?: unknown, o?: ReqOpts) => request<T>('POST', p, { ...o, body }),
  put: <T = unknown>(p: string, body?: unknown, o?: ReqOpts) => request<T>('PUT', p, { ...o, body }),
  patch: <T = unknown>(p: string, body?: unknown, o?: ReqOpts) => request<T>('PATCH', p, { ...o, body }),
  del: <T = unknown>(p: string, o?: ReqOpts) => request<T>('DELETE', p, o),
};

// ---- domain types (only the fields the UI reads; the daemon returns more) ----
export type VmStatus = 'creating' | 'running' | 'stopped' | 'paused' | 'failed' | 'hibernated' | string;
export interface Vm {
  id: string;
  name: string;
  status: VmStatus;
  backend: string;
  guest_ip?: string | null;
  labels?: Record<string, string>;
  error?: string | null;
  request: { vcpus: number; memory_mib: number; network?: unknown; apple?: { init_config?: unknown } };
}
export interface Density {
  host_pressure_level?: string;
  [k: string]: unknown;
}
