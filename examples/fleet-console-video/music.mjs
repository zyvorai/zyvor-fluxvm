// Original 90 s track: 96 bpm, A minor, Am-F-C-G, 36 bars of 2.5 s. Deterministic (seeded noise).
import { writeFileSync } from 'node:fs';
const SR = 48000, BPM = 96, BEAT = 60 / BPM, BAR = BEAT * 4, BARS = 36, DUR = BAR * BARS;
const N = Math.round(DUR * SR) + SR * 2;
const L = new Float32Array(N), R = new Float32Array(N);
let seed = 1234567;
const rnd = () => ((seed = (seed * 1664525 + 1013904223) >>> 0) / 4294967296) * 2 - 1;
const hz = (m) => 440 * Math.pow(2, (m - 69) / 12);
const CH = [[57, 60, 64, 69], [53, 57, 60, 65], [48, 52, 55, 60], [55, 59, 62, 67]]; // Am F C G (midi)
const ROOT = [33, 29, 36, 31];
const TAU = Math.PI * 2;
const add = (i, l, r) => { if (i >= 0 && i < N) { L[i] += l; R[i] += r; } };
const clamp01 = (x) => Math.max(0, Math.min(1, x));
const sect = (bar) => ({ // intensity per bar
  pad: bar < 32 ? 1 : 0.8,
  arp: bar >= 4 && bar < 34 ? (bar < 8 ? 0.55 : 1) * (bar >= 32 ? 0.6 : 1) : 0,
  beat: bar >= 8 && bar < 32,
  clap: bar >= 16 && bar < 32,
  bass: bar >= 4 && bar < 34,
});

// ---- pad: detuned additive saws, one chord per bar, slow attack, overlapping release
for (let bar = 0; bar < BARS; bar++) {
  const s = sect(bar), notes = CH[bar % 4];
  const t0 = bar * BAR, len = BAR + 0.9, i0 = Math.round(t0 * SR), n = Math.round(len * SR);
  const gain = (bar === 35 ? 1.0 : 0.9) * s.pad * (bar < 4 ? 0.7 + 0.1 * bar : 1);
  for (const [k, m] of notes.entries()) {
    const f = hz(m - 12 * (k === 3 ? 1 : 0));
    const pan = (k - 1.5) / 6;
    for (const det of [-0.0035, 0, 0.0035]) {
      let ph = Math.random() * 0; // deterministic phase 0
      const w = TAU * f * (1 + det) / SR;
      for (let j = 0; j < n; j++) {
        const t = j / SR;
        const env = Math.min(1, t / 0.7) * Math.min(1, (len - t) / 0.9);
        const lfo = 0.85 + 0.15 * Math.sin(TAU * 0.11 * (t0 + t) + k);
        const x = (Math.sin(ph) + 0.5 * Math.sin(2 * ph) + 0.25 * Math.sin(3 * ph) + 0.12 * Math.sin(4 * ph)) * env * lfo * 0.032 * gain;
        ph += w;
        add(i0 + j, x * (1 - pan), x * (1 + pan));
      }
    }
  }
}
// ---- bass: root sine, half-bar notes
for (let bar = 0; bar < BARS; bar++) {
  if (!sect(bar).bass) continue;
  for (let h = 0; h < 2; h++) {
    const t0 = bar * BAR + h * BAR / 2, i0 = Math.round(t0 * SR), n = Math.round(BAR / 2 * SR);
    const f = hz(ROOT[bar % 4] + (h ? 0 : 0));
    for (let j = 0; j < n; j++) {
      const t = j / SR, env = Math.min(1, t / 0.01) * Math.exp(-t * 1.6);
      const x = (Math.sin(TAU * f * t) + 0.3 * Math.sin(TAU * 2 * f * t)) * env * 0.22;
      add(i0 + j, x, x);
    }
  }
}
// ---- arp: 16th plucks through chord tones over two octaves, with a dotted-eighth echo
const delayBuf = [];
const PAT = [0, 1, 2, 3, 2, 1, 2, 3, 0, 2, 1, 3, 2, 1, 3, 2];
for (let bar = 0; bar < BARS; bar++) {
  const s = sect(bar); if (!s.arp) continue;
  const notes = CH[bar % 4];
  for (let st = 0; st < 16; st++) {
    const accent = st % 4 === 0 ? 1 : 0.65 + 0.15 * ((st * 7) % 3) / 2;
    const m = notes[PAT[st]] + 12 + (st >= 8 && PAT[st] < 2 ? 12 : 0);
    const f = hz(m), t0 = bar * BAR + st * BEAT / 4, i0 = Math.round(t0 * SR), n = Math.round(0.55 * SR);
    const pan = Math.sin(st * 0.9) * 0.45;
    for (const [echo, g] of [[0, 1], [BEAT * 0.75, 0.38], [BEAT * 1.5, 0.17]]) {
      const off = Math.round(echo * SR);
      for (let j = 0; j < n; j++) {
        const t = j / SR, env = Math.min(1, t / 0.003) * Math.exp(-t * 7.5);
        const x = (Math.sin(TAU * f * t) + 0.45 * Math.sin(TAU * 2 * f * t + 0.5) + 0.18 * Math.sin(TAU * 3 * f * t)) * env * 0.075 * accent * s.arp * g;
        const p = echo ? -pan : pan;
        add(i0 + off + j, x * (1 - p), x * (1 + p));
      }
    }
  }
}
// ---- drums
const kick = (t0, g) => { const i0 = Math.round(t0 * SR), n = Math.round(0.35 * SR); let ph = 0;
  for (let j = 0; j < n; j++) { const t = j / SR, f = 48 + 90 * Math.exp(-t * 28); ph += TAU * f / SR;
    const x = Math.sin(ph) * Math.exp(-t * 9) * 0.55 * g; add(i0 + j, x, x); } };
const hat = (t0, g, open) => { const i0 = Math.round(t0 * SR), n = Math.round((open ? 0.16 : 0.045) * SR); let prev = 0;
  for (let j = 0; j < n; j++) { const t = j / SR, w = rnd(), hp = w - prev; prev = w;
    const x = hp * Math.exp(-t * (open ? 22 : 90)) * 0.07 * g; add(i0 + j, x * 0.9, x * 1.1); } };
const clap = (t0, g) => { const i0 = Math.round(t0 * SR), n = Math.round(0.22 * SR); let lp = 0;
  for (let j = 0; j < n; j++) { const t = j / SR; lp += (rnd() - lp) * 0.35;
    const burst = (Math.exp(-t * 60) + 0.7 * Math.exp(-Math.max(0, t - 0.011) * 45) * (t > 0.011 ? 1 : 0)) ;
    const x = lp * burst * 0.16 * g; add(i0 + j, x, x); } };
for (let bar = 0; bar < BARS; bar++) {
  const s = sect(bar); if (!s.beat) continue;
  const b0 = bar * BAR;
  kick(b0, 1); kick(b0 + 2 * BEAT, 0.9); if (bar % 2) kick(b0 + 3.5 * BEAT, 0.6);
  for (let e = 0; e < 8; e++) hat(b0 + e * BEAT / 2, e % 2 ? 0.9 : 0.55, e === 7);
  if (s.clap) { clap(b0 + BEAT, 1); clap(b0 + 3 * BEAT, 1); }
}
// ---- riser into the outro (bars 30-31) and a swell hit at bar 32
{ const t0 = 30 * BAR, n = Math.round(2 * BAR * SR), i0 = Math.round(t0 * SR); let lp = 0;
  for (let j = 0; j < n; j++) { const t = j / (n), a = 0.02 + 0.5 * t * t; lp += (rnd() - lp) * (0.05 + 0.6 * t);
    const x = lp * a * 0.2; add(i0 + j, x, x); } }
kick(32 * BAR, 1.2);
// ---- soft master: gentle tanh, short stereo-widening haas
const out = new Int16Array(N * 2);
const hs = Math.round(0.011 * SR);
let peak = 0;
for (let i = 0; i < N; i++) { const l = L[i], r = R[i] + (i >= hs ? 0.18 * L[i - hs] : 0); peak = Math.max(peak, Math.abs(l), Math.abs(r)); }
const g = 0.9 / peak;
for (let i = 0; i < N; i++) {
  const t = i / SR; const fin = Math.min(1, t / 1.5), fout = Math.min(1, Math.max(0, (DUR + 1.5 - t) / 3));
  const l = Math.tanh(L[i] * g * 1.1) * fin * fout, r = Math.tanh((R[i] + (i >= hs ? 0.18 * L[i - hs] : 0)) * g * 1.1) * fin * fout;
  out[2 * i] = Math.round(l * 32000); out[2 * i + 1] = Math.round(r * 32000);
}
const hdr = Buffer.alloc(44), dataLen = out.length * 2;
hdr.write('RIFF', 0); hdr.writeUInt32LE(36 + dataLen, 4); hdr.write('WAVEfmt ', 8); hdr.writeUInt32LE(16, 16);
hdr.writeUInt16LE(1, 20); hdr.writeUInt16LE(2, 22); hdr.writeUInt32LE(SR, 24); hdr.writeUInt32LE(SR * 4, 28);
hdr.writeUInt16LE(4, 32); hdr.writeUInt16LE(16, 34); hdr.write('data', 36); hdr.writeUInt32LE(dataLen, 40);
writeFileSync('music-raw.wav', Buffer.concat([hdr, Buffer.from(out.buffer)]));
console.log('wrote music-raw.wav', (N / SR).toFixed(1), 's');
