// @ts-check
// Les touches de la barre de la note : des transformations pures du texte et de la sélection, sans DOM.

/** @typedef {{text: string, start: number, end: number}} EditState */

const FENCE = '```\n\n```';

/** Le début de la ligne qui contient `at`. @param {string} text @param {number} at */
const lineStart = (text, at) => text.lastIndexOf('\n', at - 1) + 1;

/** Remplace la sélection par `s`, le curseur juste après. @param {EditState} state @param {string} insert @param {number} [caret] */
function replaceSelection({ text, start, end }, insert, caret = insert.length) {
  return { text: text.slice(0, start) + insert + text.slice(end), start: start + caret, end: start + caret };
}

/**
 * Réécrit le début de chaque ligne que la sélection touche (une sélection qui finit juste après un `\n` ne touche pas la
 * ligne suivante). `change` rend le nouveau début de ligne, et le nombre de caractères retirés devant.
 * @param {EditState} s @param {(line: string) => {head: string, cut: number}} change
 */
function mapLines({ text, start, end }, change) {
  const first = lineStart(text, start);
  const last = end > start && text[end - 1] === '\n' ? end - 1 : end;
  const newline = text.indexOf('\n', last);
  const stop = newline < 0 ? text.length : newline;
  let out = '';
  let at = first;
  /** Le décalage des deux bornes : positif quand on ajoute, négatif (borné par la position) quand on retire. */
  const shift = [0, 0];
  for (const line of text.slice(first, stop).split('\n')) {
    const { head, cut } = change(line);
    [start, end].forEach((pos, i) => {
      if (pos >= at) shift[i] += head.length - Math.min(cut, pos - at);
    });
    out += `${head}${line.slice(cut)}\n`;
    at += line.length + 1;
  }
  return { text: text.slice(0, first) + out.slice(0, -1) + text.slice(stop), start: start + shift[0], end: end + shift[1] };
}

/** `#` : le caractère au curseur, à la place de la sélection. @param {EditState} s */
export const insertHash = (s) => replaceSelection(s, '#');

/** `-` : un tiret en tête de la ligne du curseur, après son retrait, une seule fois. @param {EditState} s */
export function addDash({ text, start, end }) {
  const from = lineStart(text, start);
  const at = from + (/^[ \t]*/.exec(text.slice(from))?.[0].length ?? 0);
  if (/^-( |$)/.test(text.slice(at).split('\n', 1)[0])) return { text, start, end };
  const bump = (/** @type {number} */ pos) => (pos >= at ? pos + 2 : pos);
  return { text: `${text.slice(0, at)}- ${text.slice(at)}`, start: bump(start), end: bump(end) };
}

/** `⇥` : deux espaces devant chaque ligne touchée. @param {EditState} s */
export const indent = (s) => mapLines(s, () => ({ head: '  ', cut: 0 }));

/** `⇤` : jusqu'à deux espaces (jamais de tabulation) retirés devant chaque ligne touchée. @param {EditState} s */
export const outdent = (s) => mapLines(s, (line) => ({ head: '', cut: /^ {1,2}/.exec(line)?.[0].length ?? 0 }));

/** ```` ``` ```` : un bloc de code sur ses propres lignes, le curseur à l'intérieur. @param {EditState} s */
export function insertFence(s) {
  const before = s.text.slice(0, s.start);
  const after = s.text.slice(s.end);
  const lead = before && !before.endsWith('\n') ? '\n' : '';
  const tail = after && !after.startsWith('\n') ? '\n' : '';
  return replaceSelection(s, lead + FENCE + tail, lead.length + 4);
}
