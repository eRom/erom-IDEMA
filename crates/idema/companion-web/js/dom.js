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

/** Une image : un chemin de l'app (`/logos/…`, jamais `//hôte`) ou une adresse http(s). @param {string} value */
function isImageSource(value) {
  return (value.startsWith('/') && !value.startsWith('//')) || isHttpUrl(value);
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
 * Un élément. `attrs` : `class`, attributs simples, `onclick: fonction` ; refusés : `style` (la CSP le bloque, les
 * couleurs passent par `node.style.*` ou des classes), les gestionnaires en texte, un `href` hors http(s), un `src`
 * hors chemin de l'app ou http(s). Les enfants sont du texte (jamais du HTML) ou des nœuds.
 * @template {keyof HTMLElementTagNameMap} K
 * @param {K} tag
 * @param {Record<string, unknown> | null} [attrs]
 * @param {...Child} children
 * @returns {HTMLElementTagNameMap[K]}
 */
export function el(tag, attrs, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs ?? {})) {
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
const TRAILING_PUNCTUATION = /[.,;:!?)\]}]+$/;

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
    const url = match[0].replace(TRAILING_PUNCTUATION, '');
    if (!isHttpUrl(url)) continue;
    if (match.index > last) nodes.push(document.createTextNode(text.slice(last, match.index)));
    nodes.push(el('a', { href: url, target: '_blank', rel: 'noopener noreferrer' }, url));
    last = match.index + url.length;
  }
  if (last < text.length) nodes.push(document.createTextNode(text.slice(last)));
  return nodes;
}
