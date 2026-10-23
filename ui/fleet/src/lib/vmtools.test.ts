import { describe, expect, it } from 'vitest';
import { cloneProblem, exitOf, fmtBytes, keyAction, mergeTypes, screenPoint, scrollNotches, snapshotProblem, suggestCloneName, typable } from './vmtools';

const key = (key: string, over: Partial<Parameters<typeof keyAction>[0]> = {}) => ({ key, code: '', metaKey: false, ctrlKey: false, altKey: false, shiftKey: false, ...over });

describe('snapshot and clone forms', () => {
  it('refuses what the node would', () => {
    expect(snapshotProblem('', [])).toMatch(/name/);
    expect(snapshotProblem('-bad', [])).toMatch(/starting with a letter or digit/);
    expect(snapshotProblem('a'.repeat(65), [])).toMatch(/64/);
    expect(snapshotProblem('clean', ['clean'])).toMatch(/already/);
    expect(snapshotProblem('clean', ['a', 'b'], 2)).toMatch(/at most 2/);
    expect(snapshotProblem('clean', [], 0)).toMatch(/turned off/);
    expect(snapshotProblem('clean.1_a-b', ['x'], 20)).toBe('');
  });
  it('clones from a snapshot at any time, from the disk only when stopped', () => {
    expect(cloneProblem('copy', [], '', false)).toMatch(/Stop the VM/);
    expect(cloneProblem('copy', [], '', true)).toBe('');
    expect(cloneProblem('copy', [], 'clean', false)).toBe('');
    expect(cloneProblem('copy', ['copy'], 'clean', false)).toMatch(/already exists/);
    expect(cloneProblem('a'.repeat(64), [], 'clean', false)).toMatch(/63/);
  });
  it('suggests a free name', () => {
    expect(suggestCloneName('web', [])).toBe('web-copy');
    expect(suggestCloneName('web', ['web-copy', 'web-copy-2'])).toBe('web-copy-3');
  });
  it('formats sizes', () => {
    expect(fmtBytes()).toBe('–');
    expect(fmtBytes(5 * 2 ** 20)).toBe('5 MiB');
    expect(fmtBytes(1024)).toBe('1 MiB');
    expect(fmtBytes(2 * 2 ** 30)).toBe('2 GiB');
    expect(fmtBytes(1.5 * 2 ** 30)).toBe('1.5 GiB');
  });
});

describe('screen input', () => {
  it('types printable ASCII and nothing else', () => {
    expect(typable('Hello, world!\n\t')).toBe(true);
    expect(typable('héllo')).toBe(false);
    expect(keyAction(key('a'))).toEqual({ action: 'type', text: 'a' });
    expect(keyAction(key(' '))).toEqual({ action: 'type', text: ' ' });
    expect(keyAction(key('é'))).toBeNull();
  });
  it('maps named keys, keeping only shift on a plain press', () => {
    expect(keyAction(key('Enter'))).toEqual({ action: 'key', key: 'return', modifiers: [] });
    expect(keyAction(key('Tab', { shiftKey: true }))).toEqual({ action: 'key', key: 'tab', modifiers: ['shift'] });
    expect(keyAction(key('F5'))).toEqual({ action: 'key', key: 'f5', modifiers: [] });
    expect(keyAction(key('ArrowLeft'))).toEqual({ action: 'key', key: 'left', modifiers: [] });
  });
  it('maps shortcuts to the physical key and ignores lone modifiers and composition', () => {
    expect(keyAction(key('k', { metaKey: true, code: 'KeyK' }))).toEqual({ action: 'key', key: 'k', modifiers: ['command'] });
    expect(keyAction(key('´', { altKey: true, code: 'KeyE' }))).toEqual({ action: 'key', key: 'e', modifiers: ['option'] });
    expect(keyAction(key('Enter', { metaKey: true }))).toEqual({ action: 'key', key: 'return', modifiers: ['command'] });
    expect(keyAction(key('Meta', { metaKey: true }))).toBeNull();
    expect(keyAction({ ...key('a'), isComposing: true })).toBeNull();
    expect(keyAction(key('Home', { ctrlKey: true }))?.modifiers).toEqual(['control']);
  });
  it('merges neighbouring text and respects the cap', () => {
    expect(mergeTypes([{ action: 'type', text: 'a' }, { action: 'type', text: 'b' }, { action: 'key', key: 'return' }, { action: 'type', text: 'c' }])).toEqual([
      { action: 'type', text: 'ab' }, { action: 'key', key: 'return' }, { action: 'type', text: 'c' },
    ]);
    const big = 'x'.repeat(4096);
    expect(mergeTypes([{ action: 'type', text: big }, { action: 'type', text: 'y' }])).toHaveLength(2);
  });
  it('maps a pointer into the letterboxed screenshot', () => {
    const box = { left: 0, top: 0, width: 800, height: 800 };
    const nat = { w: 1600, h: 800 }; // scale 0.5, 200px of bar above and below
    expect(screenPoint(box, nat, 400, 400)).toEqual({ x: 800, y: 400, screen_width: 1600 });
    expect(screenPoint(box, nat, 400, 100)).toBeNull();
    expect(screenPoint(box, nat, 0, 200)).toEqual({ x: 0, y: 0, screen_width: 1600 });
    expect(screenPoint({ ...box, width: 0 }, nat, 1, 1)).toBeNull();
  });
  it('turns wheel deltas into at most five notches', () => {
    expect(scrollNotches(10)).toBe(1);
    expect(scrollNotches(-100)).toBe(-3);
    expect(scrollNotches(10000)).toBe(5);
    expect(scrollNotches(0)).toBe(0);
  });
});

describe('commands', () => {
  it('reads either exit field', () => {
    expect(exitOf({ exit_code: 0 })).toBe(0);
    expect(exitOf({ exit_status: 2 })).toBe(2);
    expect(exitOf({})).toBe('?');
  });
});
