import { beforeEach, describe, expect, test } from 'bun:test';
import { createStore, safeLocalStorage } from '../js/store.js';

const FLEET = { mac: 'Mac', generated_at: 1, badge: 0, projects: [], sessions: [] };

beforeEach(() => localStorage.clear());

describe('createStore', () => {
  test('a fleet received is kept with its arrival time and wakes the subscribers', () => {
    const store = createStore(localStorage);
    let wakes = 0;
    const off = store.subscribe(() => wakes++);
    store.setFleet(FLEET, 1000);
    expect(store.state.fleet).toEqual(FLEET);
    expect(store.state.fleetAt).toBe(1000);
    expect(wakes).toBe(1);
    off();
    store.setFleet(FLEET, 2000);
    expect(wakes).toBe(1);
  });

  test('the last fleet survives a reload, so the app opens with it', () => {
    createStore(localStorage).setFleet(FLEET, 1000);
    const reopened = createStore(localStorage);
    expect(reopened.state.fleet).toEqual(FLEET);
    expect(reopened.state.fleetAt).toBe(1000);
  });

  test('a cached fleet that is not a fleet is dropped', () => {
    localStorage.setItem('idema.fleet', '{"fleet":{"sessions":"nope"},"at":5}');
    expect(createStore(localStorage).state.fleet).toBeNull();
    localStorage.setItem('idema.fleet', 'not json');
    expect(createStore(localStorage).state.fleet).toBeNull();
  });

  test('preferences are read back with their default', () => {
    const store = createStore(localStorage);
    expect(store.getPref('sort', 'state')).toBe('state');
    store.setPref('sort', 'name');
    expect(createStore(localStorage).getPref('sort', 'state')).toBe('name');
  });

  test('the last visit is persisted', () => {
    const store = createStore(localStorage);
    expect(store.state.lastVisit).toBeNull();
    store.touchVisit(4242);
    expect(createStore(localStorage).state.lastVisit).toBe(4242);
  });

  test('a storage that throws does not break the store', () => {
    const broken = /** @type {Storage} */ (/** @type {unknown} */ ({
      getItem() {
        throw new Error('denied');
      },
      setItem() {
        throw new Error('denied');
      },
    }));
    const store = createStore(broken);
    store.setFleet(FLEET, 1);
    expect(store.state.fleet).toEqual(FLEET);
  });

  test('clearing the fleet empties the state and the cache', () => {
    const store = createStore(localStorage);
    store.setFleet(FLEET, 1000);
    let wakes = 0;
    store.subscribe(() => wakes++);
    store.clearFleet();
    expect(store.state.fleet).toBeNull();
    expect(store.state.fleetAt).toBeNull();
    expect(wakes).toBe(1);
    expect(createStore(localStorage).state.fleet).toBeNull();
  });
});

describe('safeLocalStorage', () => {
  test('a browser that refuses localStorage still gives a working, empty store', () => {
    const descriptor = Object.getOwnPropertyDescriptor(globalThis, 'localStorage');
    Object.defineProperty(globalThis, 'localStorage', {
      configurable: true,
      get() {
        throw new DOMException('denied', 'SecurityError');
      },
    });
    try {
      const store = createStore(safeLocalStorage());
      store.setFleet(FLEET, 5);
      store.setPref('a', 1);
      store.clearFleet();
      expect(store.state.fleet).toBeNull();
      expect(store.getPref('a', 'fallback')).toBe('fallback');
    } finally {
      if (descriptor) Object.defineProperty(globalThis, 'localStorage', descriptor);
    }
  });
});
