#!/usr/bin/env node
// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from 'node:fs'
import { dirname, join, relative, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '../..')
try {
  const envText = readFileSync(resolve(ROOT, 'scripts/user-docs/product.env'), 'utf8')
  for (const line of envText.split('\n')) {
    const m = line.match(/^([A-Z0-9_]+)=(.*)$/)
    if (m) process.env[m[1]] = m[2].replace(/^['"]|['"]$/g, '')
  }
} catch {}
const USER_DOCS = resolve(ROOT, 'docs/user')
const SITE = resolve(process.argv[2] ?? resolve(ROOT, '../zyvor-web'))
const PRODUCT = process.env.USER_DOCS_PRODUCT || 'Hermes'
const SLUG = (process.env.USER_DOCS_SLUG || PRODUCT).toLowerCase().replace(/\s+/g, '-')
const MANUAL_DIR = process.env.USER_DOCS_MANUAL_DIR || `${SLUG}-manual`
const TARGET = join(SITE, `docs/${MANUAL_DIR}`)

if (!existsSync(join(SITE, 'docusaurus.config.ts'))) {
  console.error(`ERROR: ${SITE} is not zyvor-web`)
  process.exit(1)
}

const TOP_LEVEL_POSITION = {
  'index.md': 1,
  'getting-started.md': 2,
  'using-the-dashboard.md': 3,
  'workflows.md': 4,
  'admin-basics.md': 5,
  'page-index.md': 7,
}

const REPO_ONLY = new RegExp(
  [
        '(\\.\\./)+(handbook|guides|architecture|admin-guide|user-guide|getting-started|developer-guide|legal|client)/',
    'DEPLOYMENT_GUIDE',
    'AIRGAP_INSTALL',
    'CLI_GUIDE',
    `${SLUG}-user-feature-guide`,
    `${SLUG}-customer-feature-guide`,
    'hypercluster-user-feature-guide',
    'hypercluster-customer-feature-guide',
    'h2kvm-user-feature-guide',
    'h2kvm-customer-feature-guide',
    'guestkit-user-feature-guide',
    'guestkit-customer-feature-guide',
    'ragnarok-user-feature-guide',
    'ragnarok-customer-feature-guide',
    'axiom-user-feature-guide',
    'axiom-customer-feature-guide',
    'zyvor-fabric-user-feature-guide',
    'zyvor-fabric-customer-feature-guide',
    'hermes-user-feature-guide',
    'hermes-customer-feature-guide',
    'transiva-user-feature-guide',
    'transiva-customer-feature-guide',
    'hyper2kvm-user-feature-guide',
    'hyper2kvm-customer-feature-guide',
    'aether-user-feature-guide',
    'aether-customer-feature-guide',
    'hypersdk-user-feature-guide',
    'hypersdk-customer-feature-guide',
  ].join('|'),
)

function renameTarget(p) {
  return p.replace(/(^|\/)README\.md/, '$1index.md').replace(/(^|\/)PAGE_INDEX\.md/, '$1page-index.md')
}

function transformLinks(md) {
  return md.replace(/\[([^\]]*)\]\(([^)]+)\)/g, (full, text, target) => {
    const t = target.trim()
    if (/^(https?:|mailto:|#)/.test(t)) return full
    if (REPO_ONLY.test(t)) return text
    return `[${text}](${renameTarget(t)})`
  })
}

// The repo README links "using the CLI & API" for GitHub readers; the site has its own sidebar entry.
function stripAlsoAvailable(md) {
  return md.replace(/\nAlso available:[^\n]*\n/, '\n')
}

function frontMatter(fields) {
  const lines = ['---']
  for (const [k, v] of Object.entries(fields)) lines.push(`${k}: ${typeof v === 'string' ? JSON.stringify(v) : v}`)
  lines.push('---', '', '')
  return lines.join('\n')
}

function walk(dir) {
  const out = []
  for (const entry of readdirSync(dir)) {
    const full = join(dir, entry)
    if (statSync(full).isDirectory()) out.push(...walk(full))
    else out.push(full)
  }
  return out
}

rmSync(TARGET, { recursive: true, force: true })
mkdirSync(TARGET, { recursive: true })

let written = 0
for (const file of walk(USER_DOCS)) {
  const rel = relative(USER_DOCS, file)
  if (!rel.endsWith('.md')) continue
  const target = join(TARGET, renameTarget(rel))
  mkdirSync(dirname(target), { recursive: true })
  let body = transformLinks(readFileSync(file, 'utf8'))
  const targetName = renameTarget(rel)
  if (targetName === 'index.md') {
    body = stripAlsoAvailable(body)
    body = frontMatter({ title: `${PRODUCT} Manual`, sidebar_position: 1, slug: `/${MANUAL_DIR}` }) + body
  } else if (targetName === 'pages/index.md') {
    body = frontMatter({ title: 'Page-by-page guides', sidebar_position: 1 }) + body
  } else if (TOP_LEVEL_POSITION[targetName]) {
    body = frontMatter({ sidebar_position: TOP_LEVEL_POSITION[targetName] }) + body
  }
  writeFileSync(target, body)
  written++
}

writeFileSync(
  join(TARGET, 'pages/_category_.json'),
  JSON.stringify({ label: 'Page-by-page guides', position: 6, collapsed: true, key: `${MANUAL_DIR}-pages` }, null, 2) + '\n',
)

const pagesDir = join(TARGET, 'pages')
if (existsSync(pagesDir)) {
  let i = 2
  for (const dir of readdirSync(pagesDir).sort()) {
    const full = join(pagesDir, dir)
    if (!statSync(full).isDirectory()) continue
    const label = dir.replace(/-/g, ' ').replace(/\b\w/g, (c) => c.toUpperCase())
    writeFileSync(join(full, '_category_.json'), JSON.stringify({ label, position: i++, collapsed: true, key: `${MANUAL_DIR}-pages-${dir}` }, null, 2) + '\n')
  }
}

console.log(`Synced ${written} markdown files -> ${TARGET}`)
