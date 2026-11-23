// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { describe, expect, it } from 'vitest';
import { barHeights, formatBytes, formatCost, hours, usagePath, type UsageRow } from './usage';

const units = { running_seconds: 0, cpu_seconds: 0, memory_gib_hours: 0, disk_read_bytes: 0, disk_write_bytes: 0, net_rx_bytes: 0, net_tx_bytes: 0 };
const day = (key: string, cpu: number): UsageRow => ({ key, name: key, units: { ...units, cpu_seconds: cpu }, cost: null });

describe('usage helpers', () => {
  it('builds the query and leaves out what is empty', () => {
    expect(usagePath('vm', '', '')).toBe('/v1/usage?by=vm');
    expect(usagePath('day', '7d', 'env=dev,team')).toBe('/v1/usage?by=day&since=7d&label=env%3Ddev%2Cteam');
    expect(usagePath('label', '', '', 'team')).toBe('/v1/usage?by=label%3Ateam');
  });
  it('formats sizes, hours and costs', () => {
    expect(formatBytes(512)).toBe('512 B');
    expect(formatBytes(3 * 1024 * 1024)).toBe('3.0 MiB');
    expect(hours(5400)).toBe('1.50');
    expect(formatCost(null, 'USD')).toBe('-');
    expect(formatCost(1.5, 'EUR')).toBe('1.50 EUR');
  });
  it('scales the chart to the busiest day', () => {
    expect(barHeights([day('a', 10), day('b', 5)])).toEqual([1, 0.5]);
    expect(barHeights([day('a', 0)])).toEqual([0]);
    expect(barHeights([])).toEqual([]);
  });
});
