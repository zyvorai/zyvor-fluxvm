// A fleet VM's sheet in a real browser against the mock API: the live screen (and clicks and keys sent to it), snapshots and clones, and a
// command run in the guest. The mock records every mutating call; each step is confirmed from that record, not from a toast.
// Run through ./run.sh vmtools.
const { chromium } = require(process.env.PW_CORE || 'playwright-core');

const [, , base] = process.argv;
if (!base) { console.error('usage: vmtools.cjs http://127.0.0.1:PORT'); process.exit(2); }
const calls = async () => (await fetch(base + '/__calls')).json();
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

(async () => {
  const fails = [];
  const check = (name, cond, extra = '') => { console.log((cond ? 'ok   ' : 'FAIL ') + name + (cond ? '' : ' ' + extra)); if (!cond) fails.push(name); };
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 900 }, reducedMotion: 'reduce' });
  await ctx.addInitScript(() => sessionStorage.setItem('fluxvm.token', 'demo'));
  const page = await ctx.newPage();
  page.on('pageerror', (e) => fails.push('exception ' + e));
  page.on('console', (m) => m.type() === 'error' && !/status of 400/.test(m.text()) && fails.push('console ' + m.text().slice(0, 160)));

  await page.goto(`${base}/console/#/vms`, { waitUntil: 'load' });
  await page.getByText('ci-macos-14').first().click();
  const sheet = page.getByRole('dialog');
  await sheet.waitFor();
  const tabs = await sheet.getByRole('tab').allTextContents();
  check('a running macOS VM has Overview, Screen, Snapshots and Run', ['Overview', 'Screen', 'Snapshots', 'Run'].every((t) => tabs.includes(t)), JSON.stringify(tabs));

  // Screen and control
  await sheet.getByRole('tab', { name: 'Screen' }).click();
  const img = sheet.getByRole('img', { name: 'The screen of ci-macos-14' });
  await img.waitFor();
  await page.waitForFunction(() => { const i = document.querySelector('[role=dialog] img'); return i && i.naturalWidth === 320; });
  check('the screen shows with its size', /320 × 200/.test(await sheet.getByRole('status').innerText()));
  await sheet.getByRole('button', { name: 'Control', exact: true }).click();
  const box = await img.boundingBox();
  await page.mouse.click(box.x + box.width / 2, box.y + box.height / 2);
  await sleep(700);
  let sent = (await calls()).filter((c) => c.path === 'input').flatMap((c) => c.body.actions);
  const click = sent.find((a) => a.action === 'click');
  check('a click lands at the picture centre in the guest\'s pixels', !!click && Math.abs(click.x - 160) <= 2 && Math.abs(click.y - 100) <= 2 && click.screen_width === 320, JSON.stringify(click));
  await page.keyboard.type('hi');
  await page.keyboard.press('Enter');
  await page.keyboard.press('Meta+Space');
  await page.keyboard.press('Escape');
  await sleep(500);
  sent = (await calls()).filter((c) => c.path === 'input').flatMap((c) => c.body.actions);
  check('typed keys are merged into one type action', sent.some((a) => a.action === 'type' && a.text === 'hi'));
  check('Return is the return key', sent.some((a) => a.action === 'key' && a.key === 'return'));
  check('a shortcut carries its modifier', sent.some((a) => a.action === 'key' && a.key === 'space' && a.modifiers.includes('command')));
  check('Escape goes to the VM and does not close the sheet', sent.some((a) => a.action === 'key' && a.key === 'escape') && (await sheet.count()) === 1);
  await sheet.getByLabel('Text to type into the VM').fill('ls -la');
  await sheet.getByRole('button', { name: 'Type', exact: true }).click();
  await sleep(400);
  sent = (await calls()).filter((c) => c.path === 'input').flatMap((c) => c.body.actions);
  check('pasted text is typed', sent.some((a) => a.action === 'type' && a.text === 'ls -la'));
  await sheet.getByLabel('Text to type into the VM').fill('héllo');
  check('text the guest cannot type is refused before sending', await sheet.getByRole('alert').isVisible() && await sheet.getByRole('button', { name: 'Type', exact: true }).isDisabled());
  await sheet.getByRole('button', { name: 'Stop control' }).click();
  const before = (await calls()).length;
  await page.mouse.click(box.x + 10, box.y + 10);
  await sleep(500);
  check('after Stop control a click is not sent', (await calls()).length === before);

  // Snapshots and clones
  await sheet.getByRole('tab', { name: 'Snapshots' }).click();
  await sheet.getByText('clean-install').waitFor();
  await sheet.getByRole('button', { name: 'Take snapshot' }).first().click();
  await sheet.getByLabel('Name').fill('-bad');
  await sheet.getByRole('button', { name: 'Take snapshot' }).last().click();
  check('a bad snapshot name is refused in the form', /starting with a letter or digit/.test(await sheet.getByRole('alert').innerText()));
  await sheet.getByLabel('Name').fill('before-update');
  await sheet.getByRole('button', { name: 'Take snapshot' }).last().click();
  await sheet.getByText('before-update').waitFor();
  let c = (await calls()).find((x) => x.path === 'snapshots');
  check('a snapshot is taken with its name and memory choice', c && c.body.name === 'before-update' && c.body.include_memory === false, JSON.stringify(c));
  await sheet.getByRole('button', { name: 'Restore…' }).last().click();
  await sheet.getByRole('button', { name: 'Restore', exact: true }).click();
  await sleep(300);
  check('restore is confirmed first, then sent', (await calls()).some((x) => /^snapshots\/clean-install\/restore$/.test(x.path)));
  await sheet.getByRole('button', { name: 'Delete snapshot before-update' }).click();
  await sheet.getByRole('button', { name: 'Delete', exact: true }).click();
  await sleep(300);
  check('delete is confirmed first, then sent', (await calls()).some((x) => x.method === 'DELETE' && x.path === 'snapshots/before-update'));
  await sheet.getByRole('button', { name: 'New VM from this…' }).first().click();
  check('the clone name is suggested', (await sheet.getByLabel('Name of the new VM').inputValue()) === 'ci-macos-14-copy');
  await sheet.getByRole('button', { name: 'Create VM' }).click();
  await sleep(300);
  c = (await calls()).find((x) => x.path === 'clone');
  check('a clone names its snapshot', c && c.body.name === 'ci-macos-14-copy' && c.body.snapshot === 'clean-install', JSON.stringify(c));

  // A command
  await sheet.getByRole('tab', { name: 'Run' }).click();
  await sheet.getByLabel('Command').fill('uname -a');
  await sheet.getByRole('button', { name: 'Run', exact: true }).click();
  await sheet.getByText('Darwin mock 23.4.0').waitFor();
  check('a command shows its output and exit code', /exit 0/.test(await sheet.innerText()));
  c = (await calls()).find((x) => x.path === 'exec');
  check('the command is sent with a 60 s limit', c && c.body.command === 'uname -a' && c.body.timeout_seconds === 60, JSON.stringify(c));

  // The image catalog (operator)
  await page.keyboard.press('Escape');
  await page.goto(`${base}/console/#/images`, { waitUntil: 'load' });
  await page.getByRole('heading', { level: 1 }).waitFor();
  await page.locator('.vm-name', { hasText: 'macOS Sonoma' }).waitFor();
  await page.getByRole('button', { name: 'Add image' }).first().click();
  const form = page.getByRole('dialog');
  await form.getByLabel('ID').fill('Bad Name');
  await form.getByLabel('Disk path on the Macs').fill('/Users/fluxvm/vms/x/disk.raw');
  await form.getByRole('button', { name: 'Add image' }).click();
  check('a bad image ID is refused in the form', /lowercase/.test(await form.getByRole('alert').innerText()));
  await form.getByLabel('ID').fill('macos-27-xcode');
  await form.getByLabel('Display name').fill('macOS 27 with Xcode');
  await form.getByLabel('Default memory (GiB)').fill('16');
  await form.getByRole('button', { name: 'Add image' }).click();
  await page.locator('.vm-name', { hasText: 'macOS 27 with Xcode' }).waitFor();
  c = (await calls()).find((x) => x.path === 'images/macos-27-xcode');
  check('an image is added with memory in MiB and no empty optionals', c && c.body.memory_mib === 16384 && c.body.image === '/Users/fluxvm/vms/x/disk.raw' && c.body.guest_os === 'macos' && !('description' in c.body), JSON.stringify(c));
  await page.getByRole('button', { name: 'Edit macOS Sonoma' }).click();
  check('an existing image\'s ID cannot be changed', await page.getByRole('dialog').getByLabel('ID').evaluate((e) => e.readOnly));
  await page.getByRole('dialog').getByLabel('Default vCPUs').fill('8');
  await page.getByRole('dialog').getByRole('button', { name: 'Save changes' }).click();
  await sleep(300);
  c = (await calls()).filter((x) => x.path === 'images/macos-14').pop();
  check('an edit keeps the disk size', c && c.body.vcpus === 8 && c.body.disk_size_gib === 80, JSON.stringify(c));
  await page.getByRole('button', { name: 'Remove Ubuntu 24.04' }).click();
  await page.getByRole('dialog').getByRole('button', { name: 'Remove image' }).click();
  await sleep(300);
  check('removal is confirmed first, then sent', (await calls()).some((x) => x.method === 'DELETE' && x.path === 'images/ubuntu'));

  await browser.close();
  const unique = [...new Set(fails)];
  console.log(unique.length ? `${unique.length} failure(s)` : 'vmtools: PASS');
  process.exit(unique.length ? 1 : 0);
})().catch((e) => { console.error(e); process.exit(1); });
