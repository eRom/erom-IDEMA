import { describe, expect, test } from 'bun:test';
import { el, linkify } from '../js/dom.js';

const HOSTILE = '<img src=x onerror=alert(1)>';

describe('el', () => {
  test('hostile text becomes one text node and never an element', () => {
    const node = el('p', null, HOSTILE);
    expect(node.childNodes.length).toBe(1);
    expect(node.firstChild?.nodeType).toBe(Node.TEXT_NODE);
    expect(node.textContent).toBe(HOSTILE);
    expect(node.querySelector('img')).toBeNull();
  });

  test('hostile text in an attribute value stays a value', () => {
    const node = el('button', { 'aria-label': HOSTILE });
    expect(node.getAttribute('aria-label')).toBe(HOSTILE);
    expect(node.querySelector('img')).toBeNull();
  });

  test('a non-http href is ignored, http and https are kept', () => {
    expect(el('a', { href: 'javascript:x' }).hasAttribute('href')).toBe(false);
    expect(el('a', { href: ' JavaScript:alert(1)' }).hasAttribute('href')).toBe(false);
    expect(el('a', { href: 'data:text/html,x' }).hasAttribute('href')).toBe(false);
    expect(el('a', { href: '/idema-api/fleet' }).hasAttribute('href')).toBe(false);
    expect(el('a', { href: 'https://example.com/a?b=1' }).getAttribute('href')).toBe(
      'https://example.com/a?b=1',
    );
    expect(el('a', { href: 'http://example.com' }).getAttribute('href')).toBe('http://example.com');
  });

  test('the style attribute is refused', () => {
    const node = el('div', { style: 'color: red' });
    expect(node.hasAttribute('style')).toBe(false);
  });

  test('a string event handler is refused, a function listener works', () => {
    const inert = el('button', { onclick: 'alert(1)' });
    expect(inert.hasAttribute('onclick')).toBe(false);
    let clicks = 0;
    const live = el('button', { onclick: () => clicks++ });
    live.click();
    expect(clicks).toBe(1);
  });

  test('class, booleans, children lists and empty children', () => {
    const node = el('div', { class: 'a b', hidden: true, disabled: false }, 'x', [el('i', null, 'y'), null], 3, false);
    expect(node.className).toBe('a b');
    expect(node.hasAttribute('hidden')).toBe(true);
    expect(node.hasAttribute('disabled')).toBe(false);
    expect(node.textContent).toBe('xy3');
  });

  test('an img source is limited to same-origin paths and http(s)', () => {
    expect(el('img', { src: '/logos/claude.png' }).getAttribute('src')).toBe('/logos/claude.png');
    expect(el('img', { src: '//evil.example/x.png' }).hasAttribute('src')).toBe(false);
    expect(el('img', { src: 'javascript:x' }).hasAttribute('src')).toBe(false);
  });
});

describe('linkify', () => {
  /** @param {Node[]} nodes */
  const describeNodes = (nodes) =>
    nodes.map((node) =>
      node instanceof HTMLAnchorElement ? `a:${node.getAttribute('href')}` : `t:${node.textContent}`,
    );

  test('http and https URLs become links, the rest stays text', () => {
    const nodes = linkify('voir http://a.example/x et https://b.example/y?z=1 ok');
    expect(describeNodes(nodes)).toEqual([
      't:voir ',
      'a:http://a.example/x',
      't: et ',
      'a:https://b.example/y?z=1',
      't: ok',
    ]);
  });

  test('ftp and javascript stay text', () => {
    const text = 'ftp://x.example/a javascript:alert(1)';
    const nodes = linkify(text);
    expect(describeNodes(nodes)).toEqual([`t:${text}`]);
  });

  test('trailing punctuation is not part of the link', () => {
    const nodes = linkify('(https://a.example/x).');
    expect(describeNodes(nodes)).toEqual(['t:(', 'a:https://a.example/x', 't:).']);
  });

  test('links open outside the app without leaking the opener', () => {
    const anchor = linkify('https://a.example')[0];
    expect(anchor instanceof HTMLAnchorElement && anchor.rel).toBe('noopener noreferrer');
  });

  test('hostile markup stays text', () => {
    const nodes = linkify(`${HOSTILE} https://a.example/"><img src=x onerror=1>`);
    const holder = el('div', null, nodes);
    expect(holder.querySelector('img')).toBeNull();
    expect(holder.textContent).toContain(HOSTILE);
  });

  test('empty text gives no node', () => {
    expect(linkify('')).toEqual([]);
  });
});
