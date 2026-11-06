// @ts-check
// Les choix lus à l'écran : un menu numéroté (Claude Code, Codex) ou une question oui/non.

/** @typedef {{kind: 'menu', options: {n: number, label: string, selected: boolean}[]} | {kind: 'yesno'}} Choices */

const WINDOW = 20;
const YESNO_WINDOW = 3;
const MAX_OPTIONS = 9;
// « ❯ 1. Oui », « › 1) Oui », « 2. Non » ; Claude Code dessine parfois le menu dans un cadre (« │ ❯ 1. Oui │ »).
const OPTION = /^[\s│┃║]*([❯›>▸▶])?\s*(\d{1,2})[.)]\s+(.+?)[\s│┃║]*$/u;
const YESNO = /\[y\/n\]|\(y\/n\)|\byes\/no\b/i;

/**
 * @param {string[]} plainLines les lignes de l'écran sans séquences SGR
 * @returns {Choices | null}
 */
export function detectChoices(plainLines) {
  const lines = [...plainLines];
  while (lines.length && lines[lines.length - 1].trim() === '') lines.pop();
  // La question oui/non est tout en bas ; un menu plus haut serait un reste d'écran, elle prime.
  if (lines.slice(-YESNO_WINDOW).some((line) => YESNO.test(line))) return { kind: 'yesno' };
  const entries = lines
    .slice(-WINDOW)
    .map((line) => OPTION.exec(line))
    .filter((match) => match !== null)
    .map((match) => ({ n: Number(match[2]), label: match[3], selected: match[1] !== undefined }));
  // La dernière numérotation qui part de 1, et elle doit courir sans trou jusqu'à k (2 à 9).
  const start = entries.map((e) => e.n).lastIndexOf(1);
  const options = start < 0 ? [] : entries.slice(start);
  const valid = options.length >= 2 && options.length <= MAX_OPTIONS && options.every((o, i) => o.n === i + 1);
  return valid ? { kind: 'menu', options } : null;
}
