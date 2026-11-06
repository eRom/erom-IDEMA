import { afterEach, beforeEach, describe, expect, test } from 'bun:test';
import { mountNotes, validateLine } from '../js/notes.js';
import { createStore } from '../js/store.js';

describe('validateLine', () => {
  test('accepts a line, trimmed', () => {
    expect(validateLine('  acheter du pain  ')).toEqual({ ok: true, line: 'acheter du pain' });
  });

  test('says nothing for an empty line', () => {
    expect(validateLine('   ')).toEqual({ ok: false, error: '' });
  });

  test('refuses a tab or any control character, in French', () => {
    for (const bad of ['a\tb', 'a\u0007b', 'a\u0085b', 'a\u007fb']) {
      const result = /** @type {{ok: false, error: string}} */ (validateLine(bad));
      expect(result.ok).toBe(false);
      expect(result.error).toContain('caractère de contrôle');
    }
  });

  test('counts bytes, not characters: 2 000 bytes pass, 2 001 do not', () => {
    expect(validateLine('é'.repeat(1000)).ok).toBe(true);
    const long = /** @type {{ok: false, error: string}} */ (validateLine('é'.repeat(1000) + 'a'));
    expect(long.ok).toBe(false);
    expect(long.error).toContain('2 000');
  });
});

describe('mountNotes', () => {
  /** @type {HTMLElement} */
  let root;
  beforeEach(() => {
    localStorage.clear();
    root = document.createElement('div');
    document.body.replaceChildren(root);
  });
  afterEach(() => document.body.replaceChildren());
  const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

  /** @param {(method: string, path: string, body?: any) => any} handler */
  function mount(handler) {
    const store = createStore(localStorage);
    /** @type {{method: string, path: string, body: any}[]} */
    const calls = [];
    const request = async (/** @type {string} */ method, /** @type {string} */ path, /** @type {any} */ body) => (calls.push({ method, path, body }), handler(method, path, body));
    const unmount = mountNotes(root, /** @type {any} */ ({ store, request }));
    return { store, calls, unmount };
  }

  const input = () => /** @type {HTMLInputElement} */ (root.querySelector('input'));
  const addButton = () => /** @type {HTMLButtonElement} */ (root.querySelector('button.btn'));

  test('shows the note as text with http links, nothing more', async () => {
    mount(() => ({ kind: 'ok', data: { text: 'voir https://exemple.fr/x et <b>gras</b>\nja' + 'vascript:alert(1)', modified_at: 1 } }));
    await flush();
    const text = /** @type {HTMLElement} */ (root.querySelector('.notes__text'));
    expect(text.querySelector('a')?.getAttribute('href')).toBe('https://exemple.fr/x');
    expect(text.querySelectorAll('a').length).toBe(1);
    expect(text.querySelector('b')).toBeNull();
    expect(text.textContent).toContain('<b>gras</b>');
  });

  test('says how long ago the note changed', async () => {
    mount(() => ({ kind: 'ok', data: { text: 'x', modified_at: Date.now() - 5 * 60_000 } }));
    await flush();
    expect(root.querySelector('.notes__meta')?.textContent).toBe('Modifiée il y a 5 min');
  });

  test('a timeout keeps the line, tells to check the note, and reads it again', async () => {
    const { calls } = mount((method) => (method === 'POST' ? { kind: 'timeout' } : { kind: 'ok', data: { text: 'x', modified_at: 1 } }));
    await flush();
    input().value = 'ma ligne';
    addButton().click();
    await flush();
    expect(root.querySelector('.status')?.textContent).toBe('Le Mac ne répond pas : vérifiez la note avant de réessayer.');
    expect(input().value).toBe('ma ligne');
    expect(calls.filter((c) => c.method === 'GET').length).toBe(2);
  });

  test('scrolls to the bottom on the first load and after an append, never on a resume', async () => {
    const { calls: _calls } = mount(() => ({ kind: 'ok', data: { text: 'a\nb', modified_at: 1 } }));
    const box = /** @type {HTMLElement} */ (root.querySelector('.notes__text'));
    Object.defineProperty(box, 'scrollHeight', { value: 500 });
    await flush();
    expect(box.scrollTop).toBe(500);
    box.scrollTop = 40;
    dispatchEvent(new Event('idema:resume'));
    await flush();
    expect(box.scrollTop).toBe(40);
    input().value = 'c';
    addButton().click();
    await flush();
    expect(box.scrollTop).toBe(500);
  });

  test('adds a line, then reads the note again', async () => {
    let note = 'une';
    const { calls } = mount((method, _path, body) => {
      if (method === 'POST') {
        note += '\n' + body.line;
        return { kind: 'ok', data: { ok: true } };
      }
      return { kind: 'ok', data: { text: note, modified_at: 1 } };
    });
    await flush();
    input().value = ' deux ';
    addButton().click();
    await flush();
    expect(calls.map((c) => `${c.method} ${c.path}`)).toEqual(['GET /scratchpad', 'POST /scratchpad/append', 'GET /scratchpad']);
    expect(calls[1].body).toEqual({ line: 'deux' });
    expect(root.querySelector('.notes__text')?.textContent).toBe('une\ndeux');
    expect(input().value).toBe('');
  });

  test('a tab never leaves the phone and the field keeps the text', async () => {
    const { calls } = mount(() => ({ kind: 'ok', data: { text: '', modified_at: null } }));
    await flush();
    input().value = 'a\tb';
    addButton().click();
    await flush();
    expect(calls.some((c) => c.method === 'POST')).toBe(false);
    expect(root.querySelector('.status')?.textContent).toContain('caractère de contrôle');
    expect(input().value).toBe('a\tb');
  });

  test('a Mac that does not answer keeps the line and says so; a refusal too', async () => {
    let reply = /** @type {any} */ ({ kind: 'offline' });
    mount((method) => (method === 'POST' ? reply : { kind: 'ok', data: { text: '', modified_at: null } }));
    await flush();
    input().value = 'ma ligne';
    addButton().click();
    await flush();
    expect(root.querySelector('.status')?.textContent).toContain('injoignable');
    expect(input().value).toBe('ma ligne');
    reply = { kind: 'http', status: 400, error: 'bad_line' };
    addButton().click();
    await flush();
    expect(root.querySelector('.status')?.textContent).toContain('refusé');
  });

  test('the add button waits while the connection is down', async () => {
    const { store } = mount(() => ({ kind: 'ok', data: { text: '', modified_at: null } }));
    await flush();
    expect(addButton().disabled).toBe(false);
    store.setConnection('offline');
    expect(addButton().disabled).toBe(true);
    store.setConnection('ok');
    expect(addButton().disabled).toBe(false);
  });

  test('reads the note again when the app comes back', async () => {
    const { calls } = mount(() => ({ kind: 'ok', data: { text: 'x', modified_at: 1 } }));
    await flush();
    dispatchEvent(new Event('idema:resume'));
    await flush();
    expect(calls.filter((c) => c.method === 'GET').length).toBe(2);
  });
});
