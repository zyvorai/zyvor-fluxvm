import puppeteer from 'puppeteer-core';
import { mkdirSync } from 'node:fs';
import { pathToFileURL } from 'node:url';
const FPS = 30, DUR = 90, TOTAL = FPS * DUR, WORKERS = 4;
const only = process.argv[2] ? process.argv[2].split(',').map(Number) : null; // preview times in seconds
mkdirSync('frames', { recursive: true });
const launch = () => puppeteer.launch({ executablePath: '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', headless: 'new',
  protocolTimeout: 600000, defaultViewport: { width: 1920, height: 1080, deviceScaleFactor: 1 },
  args: ['--allow-file-access-from-files', '--force-device-scale-factor=1', '--disable-renderer-backgrounding', '--disable-background-timer-throttling'] });
const url = pathToFileURL(process.cwd() + '/scenes.html').href;
async function worker(frames, dir) {
  const b = await launch(); const p = await b.newPage(); await p.goto(url); await p.evaluate(() => window.__ready);
  for (const n of frames) {
    await p.evaluate((t) => window.__seek(t), n / FPS);
    const f = dir ? `${dir}/p${String(n).padStart(5, '0')}.png` : `frames/${String(n).padStart(5, '0')}.jpg`;
    await p.screenshot(dir ? { path: f } : { path: f, type: 'jpeg', quality: 93 });
  }
  await b.close();
}
if (only) { mkdirSync('prev', { recursive: true }); await worker(only.map((s) => Math.round(s * FPS)), 'prev'); }
else {
  const lists = Array.from({ length: WORKERS }, (_, w) => []);
  for (let n = 0; n < TOTAL; n++) lists[n % WORKERS].push(n);
  await Promise.all(lists.map((l) => worker(l)));
}
console.log('rendered');
