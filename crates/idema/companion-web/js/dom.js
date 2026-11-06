// @ts-check
// Construction du DOM sans `innerHTML` : tout texte venu du Mac passe par des nœuds texte.

/** Une adresse http ou https, et rien d'autre. @param {string} value */
function isHttpUrl(value) {
  try {
    const { protocol } = new URL(value.trim());
    return protocol === 'http:' || protocol === 'https:';
  } catch {
    return false;
  }
}

/** Une image de l'origine de la page, une fois l'adresse résolue comme le navigateur le fait. @param {string} value */
function isImageSource(value) {
  try {
    return new URL(value, location.origin).origin === location.origin;
  } catch {
    return false;
  }
}

/** @typedef {string | number | boolean | null | undefined | Node | ChildList} Child */
/** @typedef {Child[]} ChildList */

/** @param {Node} parent @param {Child} child */
function append(parent, child) {
  if (child === null || child === undefined || child === false || child === true) return;
  if (Array.isArray(child)) {
    for (const item of child) append(parent, item);
  } else if (child instanceof Node) {
    parent.appendChild(child);
  } else {
    parent.appendChild(document.createTextNode(String(child)));
  }
}

/**
 * Un élément. `attrs` : `class`, attributs simples, `onclick: fonction`. Refusés, quelle que soit la casse : `style`
 * (couleurs par `node.style.*` ou des classes), un `on*` qui n'est pas une fonction, un `href` hors http(s), un `src`
 * hors de l'origine. Les enfants sont du texte ou des nœuds, jamais du HTML.
 * @template {keyof HTMLElementTagNameMap} K
 * @param {K} tag
 * @param {Record<string, unknown> | null} [attrs]
 * @param {...Child} children
 * @returns {HTMLElementTagNameMap[K]}
 */
export function el(tag, attrs, ...children) {
  const node = document.createElement(tag);
  for (const [rawKey, value] of Object.entries(attrs ?? {})) {
    const key = rawKey.toLowerCase();
    if (value === null || value === undefined || value === false) continue;
    if (key === 'style' || key === 'srcdoc') continue;
    if (key.startsWith('on')) {
      if (typeof value === 'function') node.addEventListener(key.slice(2), /** @type {EventListener} */ (value));
      continue;
    }
    const text = value === true ? '' : String(value);
    if (key === 'href' && !isHttpUrl(text)) continue;
    if (key === 'src' && !isImageSource(text)) continue;
    if (key === 'class') node.className = text;
    else node.setAttribute(key, text);
  }
  for (const child of children) append(node, child);
  return node;
}

const URL_PATTERN = /https?:\/\/[^\s<>"'`]+/gi;
const CLOSING = /** @type {Record<string, string>} */ ({ ')': '(', ']': '[', '}': '{' });

/**
 * L'adresse sans la ponctuation de la phrase. Un seul passage (l'expression régulière était quadratique) ; une
 * parenthèse fermante reste si l'adresse porte l'ouvrante (`…/Foo_(bar)`).
 * @param {string} url
 */
function trimTrailing(url) {
  /** @type {Record<string, number>} */
  const count = {};
  for (const char of url) count[char] = (count[char] ?? 0) + 1;
  let end = url.length;
  while (end > 0) {
    const char = url[end - 1];
    if ('.,;:!?'.includes(char)) {
      end--;
    } else if (char in CLOSING && (count[char] ?? 0) > (count[CLOSING[char]] ?? 0)) {
      count[char]--;
      end--;
    } else {
      break;
    }
  }
  return url.slice(0, end);
}

/**
 * Le texte découpé en nœuds texte et en liens `<a>` pour les adresses http(s) seulement.
 * @param {string} text
 * @returns {Node[]}
 */
export function linkify(text) {
  /** @type {Node[]} */
  const nodes = [];
  let last = 0;
  for (const match of text.matchAll(URL_PATTERN)) {
    const url = trimTrailing(match[0]);
    if (!isHttpUrl(url)) continue;
    if (match.index > last) nodes.push(document.createTextNode(text.slice(last, match.index)));
    nodes.push(el('a', { href: url, target: '_blank', rel: 'noopener noreferrer' }, url));
    last = match.index + url.length;
  }
  if (last < text.length) nodes.push(document.createTextNode(text.slice(last)));
  return nodes;
}
