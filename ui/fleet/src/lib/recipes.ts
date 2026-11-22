// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

/** The recipe catalog (docs/recipes.md). A Rust test checks that every name here exists in `fluxctl recipe list`. */
export interface Recipe { name: string; title: string; summary: string; params: { key: string; value: string; help: string }[] }

export const RECIPES: Recipe[] = [
  { name: 'dev-database', title: 'Dev database', summary: 'PostgreSQL, MySQL, MongoDB or Redis in the local cluster on its usual port.', params: [{ key: 'engine', value: 'postgres', help: 'postgres, mysql, mongodb or redis' }] },
  { name: 'website', title: 'Website', summary: 'Serve a folder of static files with nginx, restarted automatically.', params: [{ key: 'dir', value: '/path/to/site', help: 'the folder with index.html' }, { key: 'port', value: '8080', help: 'port on that machine' }] },
  { name: 'ci-runner', title: 'CI runner (GitHub Actions)', summary: 'A fresh macOS VM per job on a Mac, or an ephemeral container on Linux.', params: [{ key: 'os', value: 'linux', help: 'macos or linux' }, { key: 'repo', value: 'OWNER/REPO', help: 'the repository' }] },
  { name: 'registry', title: 'Private registry', summary: 'A Docker registry with a generated password.', params: [{ key: 'port', value: '5000', help: 'port on that machine' }] },
  { name: 'monitoring', title: 'Monitoring', summary: 'Grafana for dashboards over your fleet metrics.', params: [] },
];

const plain = (v: string) => /^[A-Za-z0-9_./:=@%+,-]+$/.test(v) ? v : `'${v.replace(/'/g, `'\\''`)}'`;

/** `fluxctl recipe run NAME --set k=v ...`, the line Studio copies. */
export const runLine = (r: Recipe, values: Record<string, string>): string =>
  ['fluxctl recipe run', r.name, ...r.params.map((p) => `--set ${plain(`${p.key}=${values[p.key] ?? p.value}`)}`)].join(' ');

export const showLine = (r: Recipe): string => `fluxctl recipe show ${r.name}`;
