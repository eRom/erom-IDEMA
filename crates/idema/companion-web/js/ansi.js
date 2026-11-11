// @ts-check
// Le texte de `tmux capture-pane -e` : les séquences SGR lues en segments, puis rendues en nœuds. Tout ce qui n'est pas
// une couleur ou un attribut connu est retiré sans toucher au texte autour : jamais d'HTML, jamais d'exception.

import { el } from './dom.js';

/** La palette ANSI d'IdemA (§3.11) : normal (0 à 7), puis vif (8 à 15). */
const PALETTE = ['#1e1e20', '#ff6961', '#a8d672', '#ffd60a', '#3aa0ff', '#c58af9', '#64c3f5', '#d4d4d6', '#5a5a5e', '#ff8a80', '#c2e89a', '#ffe566', '#7dbbff', '#ddb3ff', '#9adcf9', '#ffffff'];
const CUBE = [0, 95, 135, 175, 215, 255];

/**
 * @typedef {object} Style
 * @property {string | null} fg
 * @property {string | null} bg
 * @property {boolean} bold
 * @property {boolean} dim
 * @property {boolean} italic
 * @property {boolean} underline
 * @property {boolean} inverse
 * @typedef {Style & {text: string}} Segment
 */

/** @type {Style} */
const PLAIN = { fg: null, bg: null, bold: false, dim: false, italic: false, underline: false, inverse: false };

/** @param {number} r @param {number} g @param {number} b */
const hex = (r, g, b) => '#' + [r, g, b].map((v) => Math.min(255, Math.max(0, v)).toString(16).padStart(2, '0')).join('');

/** La couleur d'un des 256 index : palette, cube 6x6x6, rampe de gris. @param {number} n */
function color256(n) {
  if (!Number.isInteger(n) || n < 0 || n > 255) return null;
  if (n < 16) return PALETTE[n];
  if (n >= 232) return hex(8 + 10 * (n - 232), 8 + 10 * (n - 232), 8 + 10 * (n - 232));
  const i = n - 16;
  return hex(CUBE[Math.floor(i / 36)], CUBE[Math.floor(i / 6) % 6], CUBE[i % 6]);
}

/** @param {number[]} codes @param {number} at l'indice du 38 ou du 48 @returns {{color: string | null, used: number}} */
function extended(codes, at) {
  if (codes[at + 1] === 5) return { color: color256(codes[at + 2]), used: 2 };
  if (codes[at + 1] === 2) {
    const rgb = codes.slice(at + 2, at + 5);
    return { color: rgb.length === 3 && rgb.every(Number.isInteger) ? hex(rgb[0], rgb[1], rgb[2]) : null, used: 4 };
  }
  return { color: null, used: 0 };
}

/** @param {Style} style @param {string} params @returns {Style} */
function applySgr(style, params) {
  const next = { ...style };
  const tokens = params === '' ? ['0'] : params.split(';');
  const codes = tokens.map((p) => (p === '' ? 0 : Number(p)));
  for (let i = 0; i < codes.length; i++) {
    const c = codes[i];
    // `4:n` : souligné de style n (courbe, pointillé…), sauf `4:0` qui l'ôte.
    if (/^4:\d$/.test(tokens[i])) next.underline = tokens[i] !== '4:0';
    else if (c === 0) Object.assign(next, PLAIN);
    else if (c === 1) next.bold = true;
    else if (c === 2) next.dim = true;
    else if (c === 3) next.italic = true;
    else if (c === 4) next.underline = true;
    else if (c === 7) next.inverse = true;
    else if (c === 22) next.bold = next.dim = false;
    else if (c === 23) next.italic = false;
    else if (c === 24) next.underline = false;
    else if (c === 27) next.inverse = false;
    else if (c >= 30 && c <= 37) next.fg = PALETTE[c - 30];
    else if (c >= 90 && c <= 97) next.fg = PALETTE[c - 82];
    else if (c === 39) next.fg = null;
    else if (c >= 40 && c <= 47) next.bg = PALETTE[c - 40];
    else if (c >= 100 && c <= 107) next.bg = PALETTE[c - 92];
    else if (c === 49) next.bg = null;
    else if (c === 38 || c === 48 || c === 58) {
      const { color, used } = extended(codes, i);
      // 58 est la couleur du soulignement : ses paramètres sont consommés, la couleur n'est pas rendue. 59 ne change rien.
      if (color && c !== 58) next[c === 38 ? 'fg' : 'bg'] = color;
      i += used;
    }
    // Tout autre code (99, 5, 53…) est ignoré ; les suivants du même ordre agissent.
  }
  // Un style identique à l'ancien en garde l'identité : les segments voisins se rejoignent.
  return /** @type {(keyof Style)[]} */ (Object.keys(PLAIN)).every((key) => next[key] === style[key]) ? style : next;
}

// Une séquence CSI (même tronquée en fin de texte), une chaîne OSC, un autre échappement, un octet de contrôle sauf
// la tabulation et le saut de ligne. Le premier groupe porte les paramètres d'une CSI, le second sa lettre finale.
// eslint-disable-next-line no-control-regex
const ESCAPES = /\x1b\[([0-?]*)[ -/]*([@-~])?|\x1b\][^\x07\x1b\n]*(?:\x07|\x1b\\)?|\x1b[ -/]*[0-~]?|[\x00-\x08\x0b-\x1f\x7f]/g;

/**
 * Les lignes d'un écran capturé, chacune en segments de même style. Le style traverse les sauts de ligne.
 * @param {string} text
 * @returns {Segment[][]}
 */
export function parseSgr(text) {
  /** @type {Segment[][]} */
  const lines = [[]];
  let style = PLAIN;
  /** Le style du dernier segment posé : tant que rien ne change, le texte s'y ajoute. @type {Style | null} */
  let last = null;

  /** @param {string} chunk */
  function add(chunk) {
    chunk.split('\n').forEach((part, index) => {
      if (index > 0) {
        lines.push([]);
        last = null;
      }
      if (part === '') return;
      const line = lines[lines.length - 1];
      if (last === style) line[line.length - 1].text += part;
      else line.push({ text: part, ...style });
      last = style;
    });
  }

  let at = 0;
  for (const match of text.matchAll(ESCAPES)) {
    add(text.slice(at, match.index));
    at = match.index + match[0].length;
    // Seule une CSI complète qui finit par `m` et ne porte que des chiffres, `;` et `:` change le style.
    if (match[2] === 'm' && /^[0-9;:]*$/.test(match[1])) style = applySgr(style, match[1]);
  }
  add(text.slice(at));
  return lines;
}

const CLASSES = /** @type {const} */ ([['bold', 'sgr-b'], ['dim', 'sgr-d'], ['italic', 'sgr-i'], ['underline', 'sgr-u']]);

// Une ligne de filet (le cadre de saisie de Claude Code, large comme le terminal du Mac) doit tenir sur une rangée du
// téléphone. Un filet est une suite d'au moins 16 traits horizontaux : en dessous, c'est un tiret de prose.
const RULE_RUN = /[─━═]{16,}/;
const RULE_RUN_GLOBAL = new RegExp(RULE_RUN.source, 'g');
// Tout autre caractère de tracé (barres, coins, jonctions) signe un tableau ou une boîte : ceux-là gardent leur rendu.
const OTHER_BOX_DRAWING = /[│-╏║-╿]/;
// Au-delà de 40 caractères hors filet, la ligne est du texte qui porte un filet, pas un filet qui porte une étiquette.
const RULE_LABEL_MAX = 40;

/** Vrai si la ligne n'est qu'un filet, avec au plus une courte étiquette. @param {Segment[]} segments */
function isRuleLine(segments) {
  const text = segments.map((seg) => seg.text).join('');
  return RULE_RUN.test(text) && !OTHER_BOX_DRAWING.test(text) && text.replace(RULE_RUN_GLOBAL, '').trim().length <= RULE_LABEL_MAX;
}

/**
 * Le nœud d'un segment : un nœud texte nu sans style, sinon un `<span>`. Les couleurs passent par le CSSOM (la CSP
 * refuse l'attribut `style` écrit en HTML), les attributs par des classes. `extraClass` force le `<span>`.
 * @param {Style} seg @param {string} text @param {string} [extraClass]
 * @returns {string | HTMLElement}
 */
function segmentNode(seg, text, extraClass = '') {
  /** @type {string[]} */
  const classes = CLASSES.filter(([key]) => seg[key]).map(([, name]) => name);
  if (extraClass) classes.push(extraClass);
  // Inversé : les couleurs s'échangent, le défaut du texte devient le fond et inversement.
  const fg = seg.inverse ? (seg.bg ?? 'var(--screen-bg)') : seg.fg;
  const bg = seg.inverse ? (seg.fg ?? 'var(--screen-text)') : seg.bg;
  if (!fg && !bg && !classes.length) return text;
  const span = el('span', { class: classes.join(' ') }, text);
  if (fg) span.style.color = fg;
  if (bg) span.style.backgroundColor = bg;
  return span;
}

/**
 * Une ligne de filet en un seul `<span>` : les longues suites de traits et les blancs de fin sont les seuls morceaux
 * qui rétrécissent (CSS), l'étiquette et le reste gardent leur largeur. Le texte total ne change pas.
 * @param {Segment[]} segments
 */
function ruleNode(segments) {
  const rule = el('span', { class: 'term__rule' });
  segments.forEach((seg, index) => {
    // Les blancs de fin de ligne ne comptent que sur le dernier segment.
    const shrinkable = index === segments.length - 1 ? new RegExp(`${RULE_RUN.source}|\\s+$`, 'g') : RULE_RUN_GLOBAL;
    let at = 0;
    for (const match of seg.text.matchAll(shrinkable)) {
      if (match.index > at) rule.append(segmentNode(seg, seg.text.slice(at, match.index)));
      rule.append(segmentNode(seg, match[0], 'term__rule-run'));
      at = match.index + match[0].length;
    }
    if (at < seg.text.length) rule.append(segmentNode(seg, seg.text.slice(at)));
  });
  return rule;
}

/**
 * Les lignes en un `<pre>`. Le texte sans style reste un nœud texte nu ; une ligne de filet est enveloppée à part.
 * @param {Segment[][]} lines
 */
export function renderScreen(lines) {
  const pre = el('pre', { class: 'term__text' });
  lines.forEach((segments, index) => {
    if (index > 0) pre.append('\n');
    if (isRuleLine(segments)) {
      pre.append(ruleNode(segments));
      return;
    }
    for (const seg of segments) pre.append(segmentNode(seg, seg.text));
  });
  return pre;
}
