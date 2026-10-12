import puppeteer from 'puppeteer-core';
import { readFileSync } from 'node:fs';
const out = readFileSync('sim.out', 'utf8');
const TEN = /Tenant token:\s+(\S+)/.exec(out)[1], OP = /Operator token:\s+(\S+)/.exec(out)[1];
const b = await puppeteer.launch({ executablePath: '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', headless: 'new',
  defaultViewport: { width: 1440, height: 810, deviceScaleFactor: 1.5 } });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
for (const [name, tok] of [['tenant', TEN], ['operator', OP]]) {
  const p = await b.newPage();
  await p.emulateMediaFeatures([{ name: 'prefers-color-scheme', value: 'dark' }]);
  await p.goto('http://127.0.0.1:18700/console', { waitUntil: 'networkidle2' });
  await p.type('input', tok); await p.keyboard.press('Enter'); await sleep(2500);
  for (const pg of ['billing', 'usage', 'overview']) {
    await p.evaluate((x) => { location.hash = '#/' + x; }, pg); await sleep(2500);
    await p.screenshot({ path: `shots/${name}-${pg}.png` });
  }
  await p.close();
}
await b.close();
