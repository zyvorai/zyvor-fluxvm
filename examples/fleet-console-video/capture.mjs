import puppeteer from 'puppeteer-core';
import { readFileSync } from 'node:fs';
const out = readFileSync('sim.out', 'utf8');
const OP = /Operator token:\s+(\S+)/.exec(out)[1], TEN = /Tenant token:\s+(\S+)/.exec(out)[1];
const BASE = 'http://127.0.0.1:18700/console';
const b = await puppeteer.launch({ executablePath: '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', headless: 'new',
  defaultViewport: { width: 1440, height: 810, deviceScaleFactor: 1.5 } });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function session(tok, name, pages, extra) {
  const p = await b.newPage();
  await p.emulateMediaFeatures([{ name: 'prefers-color-scheme', value: 'dark' }]);
  await p.goto(BASE, { waitUntil: 'networkidle2' });
  if (name === 'operator') await p.screenshot({ path: 'shots/signin.png' });
  await p.type('input', tok); await p.keyboard.press('Enter'); await sleep(2500);
  for (const pg of pages) {
    await p.evaluate((x) => { location.hash = '#/' + x; }, pg); await sleep(1400);
    await p.screenshot({ path: `shots/${name}-${pg}.png` });
  }
  if (extra) await extra(p, name);
  await p.close();
}
await session(OP, 'operator', ['overview', 'macs', 'vms', 'networks', 'images', 'volumes', 'billing', 'usage', 'tenants', 'activity', 'notifications', 'policy'],
  async (p) => {
    await p.evaluate(() => { location.hash = '#/overview'; }); await sleep(1000);
    await p.click('#navbtn-compute'); await sleep(500); await p.screenshot({ path: 'shots/menu-compute.png' });
    await p.keyboard.press('Escape');
    await p.click('#navbtn-access'); await sleep(500); await p.screenshot({ path: 'shots/menu-access.png' });
    await p.keyboard.press('Escape');
    await p.keyboard.down('Meta'); await p.keyboard.press('k'); await p.keyboard.up('Meta'); await sleep(600);
    await p.screenshot({ path: 'shots/palette.png' });
  });
await session(TEN, 'tenant', ['overview', 'macs', 'vms', 'billing', 'usage', 'members', 'keys', 'activity', 'tunnels']);
await b.close();
console.log('done');
