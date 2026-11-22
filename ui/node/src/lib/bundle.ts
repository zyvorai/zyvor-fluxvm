// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

// The support bundle download: `POST /v1/support-bundle` returns the same .tar.gz `fluxctl support-bundle` writes.

import { ApiError, apiBase, getToken } from '@fluxvm/ui-core';

/** What the user may copy and run in a terminal instead. */
export const BUNDLE_COMMAND = 'fluxctl support-bundle';

export const BUNDLE_NOTE = 'Review before sharing; secrets are removed but check for anything private.';

/** `fluxvm-support-20261122-101500.tar.gz`, from the response's Content-Disposition when it has one. */
export function bundleFileName(disposition: string | null, now: Date = new Date()): string {
  const m = disposition?.match(/filename="?([A-Za-z0-9._-]+)"?/);
  if (m) return m[1];
  const p = (n: number) => String(n).padStart(2, '0');
  return `fluxvm-support-${now.getUTCFullYear()}${p(now.getUTCMonth() + 1)}${p(now.getUTCDate())}-${p(now.getUTCHours())}${p(now.getUTCMinutes())}${p(now.getUTCSeconds())}.tar.gz`;
}

/** The sentence shown after a download: what was saved, and what was left out. */
export function bundleSummary(name: string, bytes: number, skipped: number): string {
  const size = bytes >= 1048576 ? `${(bytes / 1048576).toFixed(1)} MiB` : `${Math.max(1, Math.round(bytes / 1024))} KiB`;
  const left = skipped > 0 ? ` ${skipped} part${skipped === 1 ? ' was' : 's were'} skipped; MANIFEST.txt inside says which and why.` : '';
  return `Saved ${name} (${size}).${left} ${BUNDLE_NOTE}`;
}

/** The error text for a failed request: the daemon's own plain-English message, else what to do next. */
export function bundleFailure(status: number, message: string | null): string {
  if (message) return message;
  if (status === 403) return 'This token may not download a support bundle. Use an admin token, or run fluxctl support-bundle on the machine.';
  return `The daemon could not build the bundle (HTTP ${status}). Run ${BUNDLE_COMMAND} on the machine instead.`;
}

export interface Downloaded {
  name: string;
  bytes: number;
  skipped: number;
}

/** Asks the daemon for the bundle and hands the file to the browser. Throws an Error with a message that says what to do next. */
export async function downloadSupportBundle(): Promise<Downloaded> {
  const headers: Record<string, string> = { Accept: 'application/gzip' };
  const token = getToken();
  if (token) headers.Authorization = `Bearer ${token}`;
  let res: Response;
  try {
    res = await fetch(`${apiBase()}/v1/support-bundle`, { method: 'POST', headers, cache: 'no-store' });
  } catch {
    throw new Error(`The daemon did not answer. Run ${BUNDLE_COMMAND} on the machine instead; it works with the daemon stopped.`);
  }
  if (!res.ok) {
    let msg: string | null = null;
    try {
      msg = ((await res.json()) as { error?: string }).error ?? null;
    } catch {
      /* not JSON */
    }
    throw new ApiError(res.status, String(res.status), bundleFailure(res.status, msg));
  }
  const blob = await res.blob();
  const name = bundleFileName(res.headers.get('content-disposition'));
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = name;
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 10000);
  return { name, bytes: blob.size, skipped: Number(res.headers.get('x-fluxvm-support-skipped') ?? 0) || 0 };
}
