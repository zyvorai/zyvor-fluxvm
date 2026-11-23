// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

// The shape of `GET /v1/templates` and `GET /v1/cheatsheet` (schema 1): the one-command stacks built into fluxctl and its
// grouped commands. The same data as `fluxctl templates ls --json` and `fluxctl cheatsheet --json`.

export type TemplateKind = 'compose' | 'k3s';

export interface Template {
  name: string;
  kind: TemplateKind;
  title: string;
  summary: string;
  after: string;
  run: string;
  show: string;
}

export interface TemplateGallery {
  schema: number;
  templates: Template[];
}

export interface CheatGroup {
  group: string;
  commands: { command: string; does: string }[];
}

export interface Cheatsheet {
  schema: number;
  groups: CheatGroup[];
}

export const templatesPath = '/v1/templates';
export const cheatsheetPath = '/v1/cheatsheet';
export const runPath = (name: string) => `${templatesPath}/${encodeURIComponent(name)}/run`;

/** What a template needs before it can start, in words. */
export function needsWord(kind: TemplateKind): string {
  return kind === 'compose'
    ? 'Needs a Docker engine (on a Mac, fluxctl docker up).'
    : 'Needs a FluxVM k3s cluster (fluxctl kube up).';
}

export function kindWord(kind: TemplateKind): string {
  return kind === 'compose' ? 'Compose' : 'k3s';
}

/** Templates matching what was typed in the search box (name, title, summary or kind), in the original order. */
export function filterTemplates(all: Template[], q: string): Template[] {
  const t = q.trim().toLowerCase();
  if (!t) return all;
  return all.filter((x) => [x.name, x.title, x.summary, x.kind].some((s) => s.toLowerCase().includes(t)));
}

/** Groups with only the commands that match the search, dropping groups that end up empty. */
export function filterCheatsheet(groups: CheatGroup[], q: string): CheatGroup[] {
  const t = q.trim().toLowerCase();
  if (!t) return groups;
  return groups
    .map((g) => ({ ...g, commands: g.commands.filter((c) => c.command.toLowerCase().includes(t) || c.does.toLowerCase().includes(t)) }))
    .filter((g) => g.commands.length > 0);
}
