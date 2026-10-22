import { useEffect, useState } from 'react';
import { api } from './api';

/** What this daemon can do. Pages and actions consult this instead of guessing the host OS. */
export interface Capabilities {
  loaded: boolean;
  os: 'linux' | 'macos' | 'unknown';
  raw: Record<string, unknown>;
}

const none: Capabilities = { loaded: false, os: 'unknown', raw: {} };

export function detectOs(raw: Record<string, unknown>): Capabilities['os'] {
  const text = JSON.stringify(raw).toLowerCase();
  if (/(apple|vz|virtualization\.framework|macos)/.test(text) && !/(kvm|qemu)/.test(text)) return 'macos';
  if (/(kvm|qemu|cloud-hypervisor|firecracker)/.test(text)) return 'linux';
  return 'unknown';
}

export function useCapabilities(enabled: boolean): Capabilities {
  const [caps, setCaps] = useState<Capabilities>(none);
  useEffect(() => {
    if (!enabled) return;
    let live = true;
    api
      .get<Record<string, unknown>>('/v1/runtime/capabilities')
      .then((raw) => live && setCaps({ loaded: true, os: detectOs(raw), raw }))
      .catch(() => live && setCaps({ loaded: true, os: 'unknown', raw: {} }));
    return () => {
      live = false;
    };
  }, [enabled]);
  return caps;
}
