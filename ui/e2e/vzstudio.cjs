// Studio's fleet VM tools against a real vz guest behind a real central: the screen, snapshots (take, restore, delete), a clone from a
// snapshot, and a command in the guest. Each step is confirmed through the node's own API, not from a toast. Run through
// scripts/vz-studio-live-test.sh, which starts the stack and sets STUDIO_TOKEN, STUDIO_NODE, STUDIO_VM and STUDIO_NODE_URL.
const { chromium } = require(process.env.PW_CORE || 'playwright-core');

const [, , base] = process.argv;
const { STUDIO_TOKEN: TOKEN, STUDIO_NODE: NODE, STUDIO_VM: VM, STUDIO_NODE_URL: NODE_URL } = process.env;
if (!base || !TOKEN || !VM || !NODE_URL) { console.error('usage: vzstudio.cjs http://127.0.0.1:PORT (STUDIO_TOKEN, STUDIO_NODE, STUDIO_VM, STUDIO_NODE_URL)'); process.exit(2); }
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const central = (method, path, body) => fetch(`${base}${path}`, { method, headers: { authorization: `Bearer ${TOKEN}`, 'content-type': 'application/json' }, body: body ? JSON.stringify(body) : undefined });
const node = async (path) => (await fetch(`${NODE_URL}/v1${path}`)).json();
const until = async (what, fn, ms = 60000) => { const end = Date.now() + ms; for (;;) { const v = await fn().catch(() => null); if (v) return v; if (Date.now() > end) throw new Error('timed out waiting for ' + what); await sleep(1000); } };

(async () => {
  const fails = [];
  const check = (name, cond, extra = '') => { console.log((cond ? 'ok   ' : 'FAIL ') + name + (cond ? '' : ' ' + extra)); if (!cond) fails.push(name); };
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 900 }, reducedMotion: 'reduce' });
  await ctx.addInitScript((t) => sessionStorage.setItem('fluxvm.token', t), TOKEN);
  const page = await ctx.newPage();
  page.on('pageerror', (e) => fails.push('exception ' + e));
  page.on('console', (m) => m.type() === 'error' && !/status of 400/.test(m.text()) && fails.push('console ' + m.text().slice(0, 160)));

  await page.goto(`${base}/console/#/vms`, { waitUntil: 'load' });
  await page.locator('.vm-name', { hasText: /^studio-live$/ }).click();
  const sheet = page.getByRole('dialog');
  await sheet.waitFor();
  const tabs = await sheet.getByRole('tab').allTextContents();
  check('a running guest has Overview, Screen, Snapshots and Run', ['Overview', 'Screen', 'Snapshots', 'Run'].every((t) => tabs.includes(t)), JSON.stringify(tabs));

  // Screen
  await sheet.getByRole('tab', { name: 'Screen' }).click();
  const img = sheet.getByRole('img', { name: 'The screen of studio-live' });
  await img.waitFor({ timeout: 30000 });
  await page.waitForFunction(() => { const i = document.querySelector('[role=dialog] img'); return i && i.naturalWidth > 0; }, null, { timeout: 30000 });
  const dims = await img.evaluate((i) => [i.naturalWidth, i.naturalHeight]);
  check('the screen is the guest\'s display (960 wide, 16:10)', dims[0] === 960 && dims[1] === 600, JSON.stringify(dims));
  check('the note gives its size', /960 × 600/.test(await sheet.getByRole('status').innerText()));
  check('a Linux guest has no Control button', (await sheet.getByRole('button', { name: 'Control', exact: true }).count()) === 0);
  await sheet.getByRole('button', { name: 'Pause' }).click();
  check('the screen can be paused', (await sheet.getByRole('button', { name: 'Resume' }).count()) === 1);

  // Snapshots
  await sheet.getByRole('tab', { name: 'Snapshots' }).click();
  await sheet.getByRole('button', { name: 'Take snapshot' }).first().click();
  await sheet.getByLabel('Name').fill('-bad');
  await sheet.getByRole('button', { name: 'Take snapshot' }).last().click();
  check('a bad snapshot name is refused before it is sent', /starting with a letter or digit/.test(await sheet.getByRole('alert').innerText()));
  await sheet.getByLabel('Name').fill('ui-snap');
  await sheet.getByRole('button', { name: 'Take snapshot' }).last().click();
  await sheet.getByText('ui-snap').waitFor({ timeout: 60000 });
  let snaps = await node(`/vms/${VM}/snapshots`);
  check('the node has the snapshot, disk only', (snaps.items ?? snaps).some((s) => s.tag === 'ui-snap' && !s.has_memory), JSON.stringify(snaps).slice(0, 200));

  await sheet.getByRole('button', { name: 'New VM from this…' }).first().click();
  check('the clone name is suggested', (await sheet.getByLabel('Name of the new VM').inputValue()) === 'studio-live-copy');
  await sheet.getByRole('button', { name: 'Create VM' }).click();
  const copy = await until('the clone to exist on the node', async () => (await node('/vms')).items?.find((v) => v.name === 'studio-live-copy' && v.status === 'running') ?? (await node('/vms')).find?.((v) => v.name === 'studio-live-copy' && v.status === 'running'), 90000);
  check('the clone from the snapshot is running on the node', !!copy);

  const before = (await node(`/vms/${VM}`)).pid;
  await sheet.getByRole('button', { name: 'Restore…' }).first().click();
  await sheet.getByRole('button', { name: 'Restore', exact: true }).click();
  await until('the guest to restart', async () => { const v = await node(`/vms/${VM}`); return v.status === 'running' && v.pid !== before; }, 120000);
  check('restoring the snapshot restarted the guest', true);

  await sheet.getByRole('button', { name: 'Delete snapshot ui-snap' }).click();
  await sheet.getByRole('button', { name: 'Delete', exact: true }).click();
  await until('the snapshot to be gone', async () => { const s = await node(`/vms/${VM}/snapshots`); return !(s.items ?? s).some((x) => x.tag === 'ui-snap'); }, 30000);
  check('deleting the snapshot removes it on the node', true);

  // A command in the guest. After a restore the guest agent needs a moment, so wait until central can exec.
  await until('the guest agent after the restore', async () => (await central('POST', `/fleet/vms/${NODE}/${VM}/exec`, { command: 'true', timeout_seconds: 10 })).ok, 90000);
  await sheet.getByRole('tab', { name: 'Run' }).click();
  await sheet.getByLabel('Command').fill('uname -s');
  await sheet.getByRole('button', { name: 'Run', exact: true }).click();
  await sheet.getByText('exit 0').waitFor({ timeout: 60000 });
  check('a command runs in the guest and shows its output', /Linux/.test(await sheet.innerText()), (await sheet.innerText()).slice(-200));

  // Leave nothing behind: the clone.
  const all = await (await central('GET', '/fleet/vms')).json();
  for (const v of (all.items ?? []).filter((x) => x.name === 'studio-live-copy')) await central('DELETE', `/fleet/vms/${v.node}/${v.id}`);
  await browser.close();
  const unique = [...new Set(fails)];
  for (const u of unique) console.log('  ✗ ' + u);
  console.log(unique.length ? `${unique.length} failure(s)` : 'vzstudio: PASS');
  process.exit(unique.length ? 1 : 0);
})().catch((e) => { console.error(e); process.exit(1); });
