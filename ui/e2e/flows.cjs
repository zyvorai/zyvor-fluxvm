// UI form flows against a real central: a join token is created, validated and revoked; a host is leased to a tenant (with the
// 24-hour minimum enforced in the sheet); the billing page renders. Each flow is driven through the UI and then confirmed through the
// API, so a toast alone cannot pass it. Run through ./flows.sh (it starts a central and one simulated Mac, or uses FLOWS_URL).
const { chromium } = require(process.env.PW_CORE || 'playwright-core');

const [, , base] = process.argv;
const TOKEN = process.env.FLOWS_TOKEN || 'demo';
if (!base) { console.error('usage: flows.cjs http://127.0.0.1:PORT   (FLOWS_TOKEN=bearer)'); process.exit(2); }

const api = async (method, path, body) => {
  const r = await fetch(base + path, { method, headers: { authorization: `Bearer ${TOKEN}`, 'content-type': 'application/json' }, body: body ? JSON.stringify(body) : undefined });
  const text = await r.text();
  let json; try { json = JSON.parse(text); } catch { json = text; }
  return { status: r.status, json };
};
const rows = (j) => (Array.isArray(j) ? j : j?.items ?? []);

(async () => {
  const fails = [];
  const ok = (m) => console.log('ok   ' + m);
  const bad = (m) => { console.log('FAIL ' + m); fails.push(m); };
  const check = (name, cond, extra = '') => (cond ? ok(name) : bad(`${name} ${extra}`));

  // A tenant to lease to; 409 means an earlier run left it.
  await api('POST', '/fleet/tenants', { id: 'acme' });
  // A lease an earlier, failed run left active is released first; a released Mac then has to be scrubbed before it can be leased again (the next tenant must not inherit its disk).
  for (const l of rows((await api('GET', '/fleet/leases')).json).filter((l) => l.state === 'active' && l.tenant === 'acme')) await api('POST', `/fleet/leases/${l.id}/release?force=true`);
  for (const n of rows((await api('GET', '/fleet/nodes')).json)) await api('POST', `/fleet/nodes/${encodeURIComponent(n.name)}/scrub`);

  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 900 }, reducedMotion: 'reduce' });
  await ctx.addInitScript((t) => sessionStorage.setItem('fluxvm.token', t), TOKEN);
  const page = await ctx.newPage();
  // Central answers 400 to `/fleet/tenants/me` for the operator: that is how the app tells the roles apart.
  const expected = (url) => new URL(url).pathname === '/fleet/tenants/me';
  let expectedSeen = 0;
  page.on('console', (m) => m.type() === 'error' && !(/status of 400/.test(m.text()) && expectedSeen-- > 0) && bad('console error: ' + m.text().slice(0, 160)));
  page.on('pageerror', (e) => bad('exception: ' + String(e).slice(0, 160)));
  page.on('response', (r) => r.status() >= 400 && !(expected(r.url()) && ++expectedSeen) && bad(`HTTP ${r.status()} ${new URL(r.url()).pathname}`));
  const go = async (route) => { await page.goto(`${base}/console/#${route}`, { waitUntil: 'load' }); await page.waitForSelector('main', { timeout: 8000 }); };
  const visible = (loc) => loc.waitFor({ state: 'visible', timeout: 5000 }).then(() => true, () => false);

  // --- enrollment: validate, create, revoke -----------------------------------------------------------------
  const note = `flows ${Date.now()}`; // unique per run, so reruns against the same central find their own token
  const tokensBefore = rows((await api('GET', '/fleet/enroll/tokens')).json).length;
  await go('/enrollment');
  await page.getByRole('button', { name: 'New join token' }).click();
  await page.getByLabel('Uses').fill('0');
  check('a join token with 0 uses is refused in the sheet', await visible(page.getByText('Uses is a whole number')));
  check('and its Create button is disabled', await page.getByRole('button', { name: 'Create token' }).isDisabled());
  await page.getByLabel('Uses').fill('1');
  await page.getByLabel('Allowed host names').fill('e2e-*');
  await page.getByLabel('Note').fill(note);
  await page.getByRole('button', { name: 'Create token' }).click();
  check('the token is shown once in its own sheet', await visible(page.getByText('shown once')));
  const made = rows((await api('GET', '/fleet/enroll/tokens')).json);
  check('central has one more join token', made.length === tokensBefore + 1, `(${tokensBefore} -> ${made.length})`);
  const mine = made.find((t) => JSON.stringify(t).includes(note));
  check('it carries the pattern and the single use from the form', !!mine && JSON.stringify(mine).includes('e2e-*') && (mine.max_uses === 1 || JSON.stringify(mine).includes('"max_uses":1')), JSON.stringify(mine || made.at(-1)).slice(0, 200));
  await page.keyboard.press('Escape');
  check('the token sheet closes', await page.getByRole('dialog', { name: 'Join token' }).waitFor({ state: 'hidden', timeout: 5000 }).then(() => true, () => false));
  await go('/enrollment');
  await page.getByRole('row').filter({ hasText: note }).getByRole('button', { name: 'Revoke' }).click();
  await page.getByRole('dialog', { name: 'Revoke join token' }).getByRole('button', { name: 'Revoke' }).click();
  await page.getByRole('dialog', { name: 'Revoke join token' }).waitFor({ state: 'hidden', timeout: 5000 });
  const after = rows((await api('GET', '/fleet/enroll/tokens')).json);
  const revoked = after.find((t) => t.id === mine?.id);
  check('revoking it in the UI revokes it in central', !!revoked && (revoked.revoked === true || /revoked/.test(JSON.stringify(revoked))) , JSON.stringify(revoked).slice(0, 160));

  // --- hosts: lease a host to a tenant ----------------------------------------------------------------------
  await go('/hosts');
  const leasesBefore = rows((await api('GET', '/fleet/leases')).json).length;
  await page.getByRole('button', { name: 'Lease a host to a tenant' }).click();
  await page.getByLabel('Hours').fill('5');
  check('a 5-hour lease is refused in the sheet', await visible(page.getByText('Leases last at least 24 hours.')));
  check('and its Lease button is disabled', await page.getByRole('button', { name: 'Lease host' }).isDisabled());
  await page.getByLabel('Hours').fill('24');
  check('without a tenant the Lease button stays disabled', await page.getByRole('button', { name: 'Lease host' }).isDisabled());
  await page.getByLabel('Tenant', { exact: true }).selectOption('acme');
  await page.getByRole('button', { name: 'Lease host' }).click();
  check('the page confirms the lease', await visible(page.getByText('Host leased')));
  const leases = rows((await api('GET', '/fleet/leases')).json);
  check('central has one more lease', leases.length === leasesBefore + 1, `(${leasesBefore} -> ${leases.length})`);
  const lease = leases.filter((l) => l.tenant === 'acme' && l.state === 'active').at(-1);
  check('it belongs to acme', !!lease, JSON.stringify(leases).slice(0, 200));
  await go('/hosts');
  check('the host card counts down the 24-hour minimum', await visible(page.getByRole('meter', { name: /Minimum period ends in/ })));
  check('and Release is disabled until it is over', await page.getByRole('button', { name: /Release/ }).first().isDisabled().catch(() => false));

  // --- billing ----------------------------------------------------------------------------------------------
  await go('/billing');
  for (const t of ['Billing', 'Usage']) check(`the billing page shows its ${t} card`, await visible(page.getByText(t, { exact: true }).first()));
  // The Invoices card only appears once an invoice exists; a fresh central has none.
  const inv = await api('GET', '/fleet/billing/invoices');
  check('the invoices route answers', inv.status === 200, `(${inv.status})`);
  check(rows(inv.json).length ? 'the Invoices card shows the invoices' : 'no Invoices card while there are no invoices', (await page.getByText('Invoices', { exact: true }).count()) === (rows(inv.json).length ? 1 : 0));
  check('and no error banner', (await page.locator('[role=alert]').count()) === 0);

  if (lease) {
    check('the operator can force-release the lease', (await api('POST', `/fleet/leases/${lease.id}/release?force=true`)).status < 300);
    check('and scrub the Mac so it can be leased again', (await api('POST', `/fleet/nodes/${encodeURIComponent(lease.node)}/scrub`)).status < 300);
  }

  await browser.close();
  console.log(fails.length ? `\n${fails.length} flow check(s) failed` : '\nui flows: PASS');
  process.exit(fails.length ? 1 : 0);
})().catch((e) => { console.error('flows crashed:', e); process.exit(1); });
