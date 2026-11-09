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
