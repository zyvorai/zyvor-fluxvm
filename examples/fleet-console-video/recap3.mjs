import puppeteer from 'puppeteer-core';
import { readFileSync } from 'node:fs';
const TEN = /Tenant token:\s+(\S+)/.exec(readFileSync('sim.out', 'utf8'))[1];
const b = await puppeteer.launch({ executablePath: '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', headless: 'new',
  defaultViewport: { width: 1440, height: 810, deviceScaleFactor: 1.5 } });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const p = await b.newPage();
await p.emulateMediaFeatures([{ name: 'prefers-color-scheme', value: 'dark' }]);
await p.goto('http://127.0.0.1:18700/console', { waitUntil: 'networkidle2' });
await p.type('input', TEN); await p.keyboard.press('Enter'); await sleep(2500);
await p.evaluate(() => { location.hash = '#/vms'; }); await sleep(1500);
await p.evaluate(() => { const r = document.querySelector('tbody tr'); (r.querySelector('a,button') || r).click(); }); await sleep(1800);
await p.screenshot({ path: 'shots/tenant-vm-inspector.png' });
await b.close();
