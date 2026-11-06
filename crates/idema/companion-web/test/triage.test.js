import { describe, expect, test } from 'bun:test';
import { targetPane, triageHash, triageQueue } from '../js/triage.js';

/** @param {string} name @param {string} state @param {{id: number, active: boolean, state: string}[]} [panes] */
const session = (name, state, panes = [{ id: 0, active: true, state }]) => ({ name, state, panes });

describe('triageQueue', () => {
  test('waiting sessions first, then errors, each in the order of the bar', () => {
    const fleet = { sessions: [session('e1', 'error'), session('r', 'running'), session('w1', 'waiting'), session('i', 'idle'), session('e2', 'error'), session('w2', 'waiting')] };
    expect(triageQueue(/** @type {any} */ (fleet)).map((s) => s.name)).toEqual(['w1', 'w2', 'e1', 'e2']);
  });

  test('is empty when nothing waits and nothing failed', () => {
    expect(triageQueue(/** @type {any} */ ({ sessions: [session('r', 'running')] }))).toEqual([]);
  });
});

describe('targetPane', () => {
  test('the pane that waits, or failed, comes first', () => {
    const panes = [{ id: 0, active: true, state: 'idle' }, { id: 3, active: false, state: 'waiting' }];
    expect(targetPane(/** @type {any} */ (session('a', 'waiting', panes)))).toBe(3);
    const failed = [{ id: 0, active: true, state: 'running' }, { id: 5, active: false, state: 'error' }];
    expect(targetPane(/** @type {any} */ (session('a', 'error', failed)))).toBe(5);
  });

  test('a session in error with a waiting pane opens the pane of its own state first', () => {
    const panes = [{ id: 1, active: false, state: 'waiting' }, { id: 2, active: false, state: 'error' }];
    expect(targetPane(/** @type {any} */ (session('a', 'error', panes)))).toBe(2);
  });

  test('otherwise the active pane, then the first, then zero', () => {
    const panes = [{ id: 1, active: false, state: 'idle' }, { id: 4, active: true, state: 'running' }];
    expect(targetPane(/** @type {any} */ (session('a', 'running', panes)))).toBe(4);
    expect(targetPane(/** @type {any} */ (session('a', 'idle', [{ id: 7, active: false, state: 'idle' }])))).toBe(7);
    expect(targetPane(/** @type {any} */ (session('a', 'idle', [])))).toBe(0);
  });
});

describe('triageHash', () => {
  const fleet = /** @type {any} */ ({ sessions: [session('idema-a b', 'waiting'), session('idema-c', 'error')] });

  test('opens the first session of the queue through the router', () => {
    expect(triageHash(fleet)).toBe('#/s/idema-a%20b/0');
  });

  test('skips the sessions already handled, and rests when none is left', () => {
    expect(triageHash(fleet, ['idema-a b'])).toBe('#/s/idema-c/0');
    expect(triageHash(fleet, ['idema-a b', 'idema-c'])).toBeNull();
  });
});
