import { describe, expect, it } from 'vitest';
import { fluxctlFor, isSource, nameProblem, sizeProblem } from './images';

describe('images helpers', () => {
  it('validates names like the daemon', () => {
    expect(nameProblem('ubuntu-24.04')).toBe('');
    expect(nameProblem('')).not.toBe('');
    expect(nameProblem('.hidden')).not.toBe('');
    expect(nameProblem('a b')).not.toBe('');
    expect(nameProblem('x'.repeat(64))).not.toBe('');
  });
  it('accepts urls and absolute paths as sources', () => {
    expect(isSource('https://example.com/a.qcow2')).toBe(true);
    expect(isSource('/var/img.qcow2')).toBe(true);
    expect(isSource('relative.img')).toBe(false);
  });
  it('bounds pool size', () => {
    expect(sizeProblem('4')).toBe('');
    expect(sizeProblem('0')).not.toBe('');
    expect(sizeProblem('1.5')).not.toBe('');
    expect(sizeProblem('999')).not.toBe('');
  });
  it('quotes fluxctl arguments', () => {
    expect(fluxctlFor('catalog rm', 'a b')).toBe("fluxctl catalog rm 'a b'");
    expect(fluxctlFor('pool resize', 'p', '3')).toBe('fluxctl pool resize p 3');
  });
});
