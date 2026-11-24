// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

// The search box of a list that has labels understands what `fluxctl ... --filter label=KEY=VALUE` does:
// `label:team=ios` or `team=ios` keeps what has that label with that value, `label:team` keeps what has the label at all, and
// every other word still searches the text. All of them must match.

export type LabelTerm = { key: string; value: string | null };
export type Query = { words: string[]; labels: LabelTerm[] };

export const parseQuery = (q: string): Query => {
  const words: string[] = [];
  const labels: LabelTerm[] = [];
  for (const raw of q.trim().split(/\s+/).filter(Boolean)) {
    const t = raw.replace(/^label[:=]/i, '');
    const labelled = t !== raw || t.includes('=');
    const eq = t.indexOf('=');
    if (labelled && t && eq !== 0) labels.push(eq < 0 ? { key: t, value: null } : { key: t.slice(0, eq), value: t.slice(eq + 1) });
    else words.push(raw.toLowerCase());
  }
  return { words, labels };
};

/** True when every label term is met: the label exists, and has the value when one was given (`team=` with no value means any). */
export const labelsMatch = (labels: Record<string, string> | undefined, terms: LabelTerm[]): boolean =>
  terms.every((t) => {
    const v = labels?.[t.key];
    return v !== undefined && (t.value === null || t.value === '' || v === t.value);
  });

/** One sentence under a search box that has label support. */
export const LABEL_HINT = 'Tip: type team=ios (or label:team) to keep what carries that label, like fluxctl --filter label=team=ios.';

/** `label=team=ios` for each term, to paste after `--filter` (what the box currently means in fluxctl). */
export const filterFlags = (q: string): string =>
  parseQuery(q).labels.map((l) => `--filter label=${l.key}${l.value ? '=' + l.value : ''}`).join(' ');
