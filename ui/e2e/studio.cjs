// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

// Studio page smoke: Home (Self-test and cluster operations cards), Events, Audit, Usage, Mac, Apps (create form) and the Mac fleet
// (label filter), each against the mock node API. Fails on console errors, page errors, failed requests and missing key elements.
// Run through ./run.sh studio (it installs playwright-core once and starts the mock). Set PW_CHROME to a Chrome binary to use it.
const { chromium } = require(process.env.PW_CORE || 'playwright-core');

const [, , base] = process.argv;
if (!base) {
  console.error('usage: studio.cjs http://127.0.0.1:PORT');
  process.exit(2);
}

// Each case: where to go, what must be visible, and an optional interaction with its own expectations.
const CASES = [
  { name: 'Home', path: '/', see: ['Cluster operations: dev', 'Self-test'], run: async (page, ok) => {
    await page.getByRole('button', { name: /^Run$/ }).first().click();
    await ok(page.getByText('5 passed, 1 warning'), 'self-test result');
    await ok(page.getByText('What to do: free some space'), 'self-test advice');
    await page.getByRole('button', { name: /Upgrade k3s/ }).click();
    await ok(page.getByText('Upgrade dev from v1.33.1 to v1.34.0'), 'upgrade plan preview');
    await ok(page.getByRole('button', { name: 'Back up and upgrade' }), 'typed-confirmation upgrade button');
  } },
  { name: 'Alerts', path: '/alerts', see: ['Backup bk-7 did not finish.', 'Mac instance mac-runner-1 is unhealthy.', 'fluxctl explain MAC_BACKUP_IO'], run: async (page, ok) => {
    await ok(page.getByText('1 error and 1 warning need attention').first(), 'headline counts');
    await ok(page.getByTestId('alerts-badge'), 'bell badge');
    await page.getByTestId('alerts-bell').click();
    await ok(page.getByRole('dialog', { name: 'Alerts' }), 'bell dropdown');
    await page.keyboard.press('Escape');
    await page.getByRole('button', { name: 'Snooze 24h' }).first().click();
    await ok(page.getByText('1 warning needs attention').first(), 'snoozing an alert lowers the count');
    await page.getByRole('button', { name: 'Snoozed: hidden' }).click();
    await ok(page.getByRole('button', { name: 'Bring back' }), 'snoozed alerts can be shown again');
  } },
  { name: 'Events', path: '/events', see: ['web-01 started', 'build-mac run failed'] },
  { name: 'Audit', path: '/audit', see: ['vm.create', 'mac.rm'] },
  { name: 'Usage', path: '/usage', see: ['web-01'] },
  { name: 'Mac', path: '/mac', see: ['macOS instances', 'mac-runner-1'] },
  { name: 'Apps', path: '/apps', see: ['Run an app', 'image pull failing'], run: async (page, ok) => {
    for (const l of ['Image', 'Name', 'Port', 'Labels', 'Copies']) await ok(page.getByLabel(l, { exact: true }), `form field "${l}" has a label`);
    await page.getByLabel('Filter apps').fill('team=ios');
    await ok(page.getByText('nginx'), 'label filter keeps the ios app');
    await page.getByText('image pull failing').waitFor({ state: 'detached', timeout: 3000 });
  } },
  { name: 'Mac fleet', path: '/mac-fleet', see: ['mini-lab', 'mini-prod'], run: async (page, ok) => {
    await page.getByLabel('Filter Macs').fill('zone=lab');
    await ok(page.getByText('mini-lab'), 'label filter keeps mini-lab');
    await page.getByText('mini-prod').waitFor({ state: 'detached', timeout: 3000 });
  } },
];

(async () => {
  const launch = process.env.PW_CHROME ? { executablePath: process.env.PW_CHROME } : { channel: 'chrome' };
  const browser = await chromium.launch({ ...launch, headless: true });
  const findings = [];
  for (const c of CASES) {
    const ctx = await browser.newContext({ viewport: { width: 1280, height: 900 }, reducedMotion: 'reduce' });
    await ctx.addInitScript(() => { try { sessionStorage.setItem('fluxvm.token', 'demo'); } catch { /* about:blank */ } });
    const page = await ctx.newPage();
    const bad = (m) => findings.push(`${c.name}: ${m}`);
    page.on('console', (m) => m.type() === 'error' && bad('console ' + m.text().slice(0, 160)));
    page.on('pageerror', (e) => bad('exception ' + String(e).slice(0, 160)));
    page.on('response', (r) => r.status() >= 400 && bad(`HTTP ${r.status()} ${new URL(r.url()).pathname}`));
    const ok = async (loc, what) => { await loc.filter({ visible: true }).first().waitFor({ timeout: 6000 }).catch(() => bad(`missing: ${what}`)); };
    try {
      await page.goto(`${base}/console/#${c.path}`, { waitUntil: 'load' });
      await page.waitForSelector('main', { timeout: 8000 }).catch(() => bad('no main element rendered'));
      for (const t of c.see) await ok(page.getByText(t, { exact: false }), `text "${t}"`);
      if (c.run) await c.run(page, ok);
      await page.waitForTimeout(500);
    } catch (e) {
      bad('interaction failed: ' + String(e).split('\n')[0].slice(0, 200));
    }
    console.log(`${findings.some((f) => f.startsWith(c.name + ':')) ? 'FAIL' : 'ok  '} ${c.name}`);
    await ctx.close();
  }
  await browser.close();
  const unique = [...new Set(findings)];
  for (const f of unique) console.error('  x ' + f);
  console.log(`${CASES.length} pages checked, ${unique.length} finding(s)`);
  process.exit(unique.length ? 1 : 0);
})().catch((e) => {
  console.error(e);
  process.exit(3);
});
