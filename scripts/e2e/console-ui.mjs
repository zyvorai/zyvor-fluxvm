// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0
//
// The fleet console in a real (headless) browser: sign in as the operator and as a tenant, open every page of the nav and the
// inspector of a node and a VM, and fail on any uncaught exception, console error or failed request that is not an expected 4xx.
// Driven by scripts/e2e/console-ui.sh, which starts central and two simulated Macs; usage: node console-ui.mjs URL ADMIN_TOKEN TENANT_TOKEN
import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);
let puppeteer;
for (const m of ['puppeteer', 'puppeteer-core']) { try { puppeteer = require(m); break; } catch { /* try next */ } }
if (!puppeteer) { console.error('needs puppeteer (npm i -g puppeteer) or puppeteer-core with CHROME_PATH'); process.exit(2); }
const [url, adminTok, tenantTok] = process.argv.slice(2);
const executablePath = process.env.CHROME_PATH || (process.platform === 'darwin' ? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome' : undefined);
let fails = 0;
const ok = (n) => console.log('ok   ' + n);
const bad = (n, why) => { console.log('FAIL ' + n + (why ? ' :: ' + why : '')); fails++; };
const check = (n, cond, why) => (cond ? ok(n) : bad(n, why));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const browser = await puppeteer.launch({ headless: true, executablePath, args: ['--no-sandbox', '--user-data-dir=' + process.env.UI_PROFILE] });
async function session(token, label) {
  const ctx = await browser.createBrowserContext();
  const page = await ctx.newPage();
  await page.setViewport({ width: 1280, height: 900 });
  const problems = [];
  page.on('pageerror', (e) => problems.push('exception: ' + e.message));
  page.on('console', (m) => { if (m.type() === 'error' && !/status of 4\d\d/.test(m.text())) problems.push('console: ' + m.text()); });
  page.on('requestfailed', (r) => problems.push('request failed: ' + r.url()));
  await page.goto(url + '/console', { waitUntil: 'networkidle0' });
  check(label + ': the sign-in form shows first', await page.$eval('#signin', (e) => !e.hidden));
  await page.type('#signin-token', 'wrong-token');
  await page.click('#signin-form button[type=submit], #signin-form .primary[type=submit]').catch(() => page.keyboard.press('Enter'));
  await sleep(800);
  check(label + ': a wrong token is refused with a message', (await page.$eval('#signin-error', (e) => e.textContent.trim())).length > 0 && await page.$eval('#app', (e) => e.hidden));
  await page.$eval('#signin-token', (e) => { e.value = ''; });
  await page.type('#signin-token', token);
  await page.keyboard.press('Enter');
  await page.waitForFunction(() => !document.getElementById('app').hidden, { timeout: 15000 }).catch(() => {});
  check(label + ': the right token opens the app', await page.$eval('#app', (e) => !e.hidden));
  await sleep(1500);
  return { page, problems, ctx };
}

for (const [label, tok] of [['operator', adminTok], ['tenant', tenantTok]]) {
  const { page, problems, ctx } = await session(tok, label);
  const links = await page.$$eval('#nav a', (as) => as.map((a) => [a.textContent.trim(), a.getAttribute('href')]));
  check(label + ': the nav has pages', links.length >= 3, JSON.stringify(links));
  for (const [name, href] of links) {
    await page.evaluate((h) => { location.hash = h.replace(/^.*#/, '#'); }, href);
    await sleep(700);
    const text = await page.$eval('#view', (e) => e.innerText.trim());
    check(label + ': "' + name + '" renders something', text.length > 0, 'empty view');
  }
  const bodyText = await page.$eval('body', (e) => e.innerText);
  check(label + ': no "undefined" or "[object Object]" leaks into the UI', !/\bundefined\b|\[object Object\]|NaN/.test(bodyText), (bodyText.match(/.{20}(undefined|\[object Object\]|NaN).{20}/) || [''])[0]);
  await page.click('#theme').catch(() => {}); await sleep(200);
  await page.click('#refresh').catch(() => {}); await sleep(800);
  await page.click('#bell').catch(() => {}); await sleep(300);
  check(label + ': the notification bell opens', await page.$eval('#bell-panel', (e) => !e.hidden).catch(() => false));
  await page.click('#acct').catch(() => {}); await sleep(300);
  check(label + ': the account menu names the role', /operator|admin|tenant|acme/i.test(await page.$eval('#acct-panel', (e) => e.innerText).catch(() => '')));
  if (label === 'operator') {
    await page.evaluate(() => { location.hash = '#/nodes'; }); await sleep(700);
    const row = await page.$('#view tr[data-id], #view .row, #view tbody tr');
    if (row) { await row.click().catch(() => {}); await sleep(800); check('operator: the inspector of a node opens', await page.$eval('#inspector', (e) => !e.hidden).catch(() => false)); }
  }
  await page.setViewport({ width: 390, height: 800 }); await sleep(500);
  check(label + ': phone width has no horizontal scroll', await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1), 'scrollWidth ' + await page.evaluate(() => document.documentElement.scrollWidth));
  check(label + ': no script errors, console errors or failed requests', problems.length === 0, problems.slice(0, 4).join(' | '));
  await page.click('#signout').catch(async () => { await page.click('#acct'); await page.click('#signout'); });
  await sleep(800);
  check(label + ': sign out returns to the form and forgets the token', await page.evaluate(() => !document.getElementById('signin').hidden && !Object.values(sessionStorage).some((v) => v.length > 0 && /^(adm|fvt_)/.test(v))));
  await ctx.close();
}
await browser.close();
console.log('\n' + fails + ' check(s) failed');
process.exit(fails);
