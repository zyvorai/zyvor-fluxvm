// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

import { describe, expect, it } from 'vitest';
import { resizeMessage, wsUrl } from './ws';

describe('wsUrl', () => {
  it('maps http(s) to ws(s) and keeps a proxy prefix', () => {
    expect(wsUrl('/console/ws/abc?cols=80', { protocol: 'http:', host: 'h:7788' }, '')).toBe('ws://h:7788/console/ws/abc?cols=80');
    expect(wsUrl('/console/ws/abc', { protocol: 'https:', host: 'h' }, '/flux')).toBe('wss://h/flux/console/ws/abc');
  });
});
describe('resizeMessage', () => {
  it('is the JSON the daemon expects', () => {
    expect(JSON.parse(resizeMessage(120, 40))).toEqual({ cols: 120, rows: 40 });
    expect(JSON.parse(resizeMessage(120, 40, 'nested'))).toEqual({ resize: { cols: 120, rows: 40 } });
  });
});
