// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

// The shape of `GET /v1/usage` (the same data as `fluxctl usage --json`) and the small helpers the Usage page needs.

export interface Units {
  running_seconds: number;
  cpu_seconds: number;
  memory_gib_hours: number;
  disk_read_bytes: number;
  disk_write_bytes: number;
  net_rx_bytes: number;
  net_tx_bytes: number;
}

export interface UsageRow {
  key: string;
  name: string;
  units: Units;
  cost: number | null;
}

export interface UsageReport {
  by: string;
  since: string | null;
  priced: boolean;
  currency: string;
  total: Units;
  total_cost: number | null;
  rows: UsageRow[];
  days: UsageRow[];
}

export const NO_RATES_NOTE = 'No rates set, showing units only. To see costs run: fluxctl config set usage.cpu_hour 0.04 (also usage.memory_gib_hour, usage.disk_gib, usage.network_gib, usage.currency).';

/** `/v1/usage?by=..&since=..&label=..`, leaving out what is empty. */
export function usagePath(by: string, since: string, label: string, key = ''): string {
  const q = [`by=${encodeURIComponent(by === 'label' ? `label:${key.trim()}` : by)}`];
  if (since) q.push(`since=${encodeURIComponent(since)}`);
  if (label.trim()) q.push(`label=${encodeURIComponent(label.trim())}`);
  return `/v1/usage?${q.join('&')}`;
}

export function formatBytes(n: number): string {
  const u = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < u.length - 1) {
    v /= 1024;
    i++;
  }
  return i === 0 ? `${n} B` : `${v.toFixed(1)} ${u[i]}`;
}

export const hours = (secs: number): string => (secs / 3600).toFixed(2);

export function formatCost(c: number | null, currency: string): string {
  return c === null ? '-' : `${c.toFixed(2)} ${currency}`;
}

/** Bar heights (0..1) for the daily CPU hours; the largest day is 1. All zero for no usage. */
export function barHeights(days: UsageRow[]): number[] {
  const max = Math.max(0, ...days.map((d) => d.units.cpu_seconds));
  return days.map((d) => (max > 0 ? d.units.cpu_seconds / max : 0));
}
