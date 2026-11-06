import { describe, expect, test } from 'bun:test';
import { ageFr, relativeFr } from '../js/time.js';

const NOW = 1_800_000_000_000;
const MIN = 60_000;
const HOUR = 60 * MIN;

describe('relativeFr', () => {
  test('under a minute, or in the future, is right now', () => {
    expect(relativeFr(NOW, NOW)).toBe("à l'instant");
    expect(relativeFr(NOW - 59_000, NOW)).toBe("à l'instant");
    expect(relativeFr(NOW + 5 * MIN, NOW)).toBe("à l'instant");
  });

  test('minutes, hours, yesterday, days', () => {
    expect(relativeFr(NOW - 3 * MIN, NOW)).toBe('3 min');
    expect(relativeFr(NOW - 59 * MIN, NOW)).toBe('59 min');
    expect(relativeFr(NOW - 2 * HOUR - 10 * MIN, NOW)).toBe('2 h');
    expect(relativeFr(NOW - 23 * HOUR, NOW)).toBe('23 h');
    expect(relativeFr(NOW - 30 * HOUR, NOW)).toBe('hier');
    expect(relativeFr(NOW - 72 * HOUR, NOW)).toBe('3 j');
  });
});

describe('ageFr', () => {
  test('the age of data, in words that follow « il y a »', () => {
    expect(ageFr(NOW - 20_000, NOW)).toBe("moins d'une minute");
    expect(ageFr(NOW - 5 * MIN, NOW)).toBe('5 min');
    expect(ageFr(NOW - 3 * HOUR, NOW)).toBe('3 h');
    expect(ageFr(NOW - 50 * HOUR, NOW)).toBe('2 j');
    expect(ageFr(NOW + MIN, NOW)).toBe("moins d'une minute");
  });
});
