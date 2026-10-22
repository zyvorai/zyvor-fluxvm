// Helpers for the guest-agent style responses shared by VM exec, sandbox process and agent-session exec.
export interface ExecResult {
  result?: string;
  exit_code?: number | null;
  stdout?: string;
  stderr?: string;
  message?: string;
  output_truncated?: boolean;
  enforcement?: { not_enforced?: string[] } & Record<string, unknown>;
}

/** The daemon can answer HTTP 200 with `{"result":"error","message":...}`; surface that as a failure. */
export function execFailure(r: ExecResult): string {
  if (r.result === 'error') return r.message || 'The guest agent reported an error.';
  return '';
}

export const execTone = (r: ExecResult): 'ok' | 'bad' | 'idle' => (r.exit_code == null ? 'idle' : r.exit_code === 0 ? 'ok' : 'bad');

export function toBase64(bytes: Uint8Array): string {
  let s = '';
  for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(s);
}
export function fromBase64(b64: string): Uint8Array {
  const s = atob(b64);
  const out = new Uint8Array(s.length);
  for (let i = 0; i < s.length; i++) out[i] = s.charCodeAt(i);
  return out;
}

export const MAX_UPLOAD_BYTES = 8 * 1024 * 1024;

/** Where the file name comes from when saving `/var/log/syslog`. */
export const baseName = (path: string): string => path.replace(/\/+$/, '').split('/').pop() || 'download';

export function isAbsPath(p: string): boolean {
  return p.startsWith('/') && p.length <= 4096 && !p.includes('\0') && !p.includes('\n') && !p.split('/').includes('..');
}

export function download(name: string, bytes: Uint8Array, type = 'application/octet-stream'): void {
  const url = URL.createObjectURL(new Blob([bytes as BlobPart], { type }));
  const a = document.createElement('a');
  a.href = url;
  a.download = name;
  a.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

export const fmtUnix = (s?: number | null): string => (s ? new Date(s * 1000).toLocaleString() : '—');
