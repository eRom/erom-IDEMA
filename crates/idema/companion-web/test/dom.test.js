import { describe, expect, test } from 'bun:test';
import { el } from '../js/dom.js';

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

  test('the guards hold whatever the case of the attribute key', () => {
    expect(el('a', { Href: 'javascript:x' }).hasAttribute('href')).toBe(false);
    expect(el('a', { HREF: 'javascript:x' }).hasAttribute('href')).toBe(false);
    expect(el('div', { Style: 'color: red' }).hasAttribute('style')).toBe(false);
    expect(el('div', { STYLE: 'color: red' }).hasAttribute('style')).toBe(false);
    expect(el('button', { ONCLICK: 'alert(1)' }).hasAttribute('onclick')).toBe(false);
    expect(el('button', { OnMouseOver: 'alert(1)' }).hasAttribute('onmouseover')).toBe(false);
    expect(el('iframe', { SrcDoc: '<script>1</script>' }).hasAttribute('srcdoc')).toBe(false);
    expect(el('img', { SRC: '//evil.example/x.png' }).hasAttribute('src')).toBe(false);
    expect(el('a', { Href: 'https://ok.example/' }).getAttribute('href')).toBe('https://ok.example/');
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

  test('an image source that resolves to another origin is refused, a backslash included', () => {
    for (const src of ['/\\evil.example/x.png', '\\\\evil.example/x.png', 'https://evil.example/x.png', 'data:image/png;base64,AAAA']) {
      expect(el('img', { src }).hasAttribute('src')).toBe(false);
    }
    expect(el('img', { src: `${location.origin}/logos/a.png` }).hasAttribute('src')).toBe(true);
  });
});
