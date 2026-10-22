// UI smoke: every page of an app, at phone and desktop width, in both themes, against its mock API.
// Fails on console errors, page errors, failed requests, horizontal overflow, controls without an accessible name,
// and a page without exactly one <h1>. Run through ./run.sh (it installs playwright-core once and starts the mock).
const { chromium } = require(process.env.PW_CORE || 'playwright-core');

const [, , app, base, ...rest] = process.argv;
const PAGES = {
  node: ['/', '/machines', '/machines/vm-0', '/sandboxes', '/agents', '/network', '/network?tab=vnets', '/network?tab=groups', '/network?tab=services', '/network?tab=flows', '/images', '/volumes', '/branches', '/backups', '/host', '/access'],
  fleet: ['/', '/hosts', '/vms', '/networks', '/tenants', '/billing', '/activity'],
}[app];
if (!PAGES || !base) {
  console.error('usage: smoke.cjs node|fleet http://127.0.0.1:PORT [--widths 390,1280]');
  process.exit(2);
}
const widths = (rest.includes('--widths') ? rest[rest.indexOf('--widths') + 1] : '390,1280').split(',').map(Number);

(async () => {
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  const findings = [];
  let checked = 0;
  for (const theme of ['dark', 'light']) {
    for (const width of widths) {
      const ctx = await browser.newContext({ viewport: { width, height: 900 }, reducedMotion: 'reduce' });
      await ctx.addInitScript(([t]) => {
        sessionStorage.setItem('fluxvm.token', 'demo');
        localStorage.setItem('fluxvm-theme', t);
      }, [theme]);
      for (const p of PAGES) {
        const page = await ctx.newPage();
        const where = `${p} @${width}px ${theme}`;
        const bad = (m) => findings.push(`${where}: ${m}`);
        // Central answers 400 to `/fleet/tenants/me` for the operator: that is how the app tells the roles apart.
        const expected = (url) => app === 'fleet' && new URL(url).pathname === '/fleet/tenants/me';
        let expectedSeen = 0;
        page.on('console', (m) => m.type() === 'error' && !(/status of 400/.test(m.text()) && expectedSeen-- > 0) && bad('console ' + m.text().slice(0, 160)));
        page.on('pageerror', (e) => bad('exception ' + String(e).slice(0, 160)));
        page.on('response', (r) => r.status() >= 400 && !(expected(r.url()) && ++expectedSeen) && bad(`HTTP ${r.status()} ${new URL(r.url()).pathname}`));
        await page.goto(`${base}/console/#${p}`, { waitUntil: 'load' });
        await page.waitForSelector('main', { timeout: 8000 }).catch(() => bad('no <main> rendered'));
        await page.waitForTimeout(900);
        const r = await page.evaluate(() => {
          const out = { overflow: document.documentElement.scrollWidth - window.innerWidth, h1: document.querySelectorAll('h1').length, unnamed: [] };
          const named = (el) => {
            if (el.getAttribute('aria-label') || el.getAttribute('aria-labelledby') || el.getAttribute('title')) return true;
            if ((el.textContent || '').trim()) return true;
            if (el.id && document.querySelector(`label[for="${el.id}"]`)) return true;
            return !!el.closest('label');
          };
          for (const el of document.querySelectorAll('button, a[href], input:not([type=hidden]), select, textarea')) {
            if (!named(el) && !(el.getAttribute('placeholder'))) out.unnamed.push(el.tagName.toLowerCase() + (el.className ? '.' + String(el.className).split(' ')[0] : ''));
          }
          return out;
        });
        if (r.overflow > 1) bad(`horizontal overflow by ${r.overflow}px`);
        if (r.h1 !== 1) bad(`${r.h1} <h1> elements`);
        for (const u of [...new Set(r.unnamed)]) bad(`control without an accessible name: ${u}`);
        checked++;
        await page.close();
      }
      await ctx.close();
    }
  }
  await browser.close();
  const unique = [...new Set(findings)];
  for (const f of unique) console.error('  ✗ ' + f);
  console.log(`${checked} page views checked, ${unique.length} finding(s)`);
  process.exit(unique.length ? 1 : 0);
})().catch((e) => {
  console.error(e);
  process.exit(3);
});
