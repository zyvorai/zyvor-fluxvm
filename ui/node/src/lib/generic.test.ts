import { describe, expect, it } from 'vitest';
import { columnsOf, rowsOf } from './generic';

describe('rowsOf', () => {
  it('finds arrays at the top level or under conventional keys', () => {
    expect(rowsOf([{ a: 1 }])).toEqual([{ a: 1 }]);
    expect(rowsOf({ flows: [{ a: 1 }] })).toEqual([{ a: 1 }]);
    expect(rowsOf({ custom: [{ b: 2 }] }, 'custom')).toEqual([{ b: 2 }]);
    expect(rowsOf({ nope: 1 })).toEqual([]);
    expect(rowsOf(null)).toEqual([]);
  });
});

describe('columnsOf', () => {
  it('keeps the most common scalar columns and skips objects', () => {
    const rows = [{ a: 1, b: 2, nested: { x: 1 } }, { a: 3, c: 4 }];
    expect(columnsOf(rows)).toEqual(['a', 'b', 'c']);
    expect(columnsOf(rows, 1)).toEqual(['a']);
  });
});
