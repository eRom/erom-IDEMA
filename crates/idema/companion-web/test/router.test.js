import { describe, expect, test } from 'bun:test';
import { parseRoute, sessionHash } from '../js/router.js';

describe('parseRoute', () => {
  test('the fleet is the home route', () => {
    for (const hash of ['', '#', '#/']) {
      expect(parseRoute(hash)).toEqual({ name: 'fleet' });
    }
  });

  test('absence and notes', () => {
    expect(parseRoute('#/absence')).toEqual({ name: 'absence' });
    expect(parseRoute('#/notes')).toEqual({ name: 'notes' });
  });

  test('a session route carries its tmux name and pane', () => {
    expect(parseRoute('#/s/idema-1a2b3c4d/2')).toEqual({
      name: 'session',
      session: 'idema-1a2b3c4d',
      pane: 2,
    });
  });

  test('an encoded tmux name is decoded', () => {
    expect(parseRoute('#/s/a%20b%2Fc/0')).toEqual({ name: 'session', session: 'a b/c', pane: 0 });
  });

  test('sessionHash round-trips through parseRoute', () => {
    const route = parseRoute(sessionHash('we ird/name', 7));
    expect(route).toEqual({ name: 'session', session: 'we ird/name', pane: 7 });
  });

  test('anything unreadable falls back to the fleet', () => {
    for (const hash of ['#/s/x', '#/s/x/notanumber', '#/s//1', '#/s/%E0%A4%A/1', '#/nope', '#/s/x/1/extra']) {
      expect(parseRoute(hash)).toEqual({ name: 'fleet' });
    }
  });
});
