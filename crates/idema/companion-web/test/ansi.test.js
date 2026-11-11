import { describe, expect, test } from 'bun:test';
import { parseSgr, renderScreen } from '../js/ansi.js';

const NORMAL = ['#1e1e20', '#ff6961', '#a8d672', '#ffd60a', '#3aa0ff', '#c58af9', '#64c3f5', '#d4d4d6'];
const BRIGHT = ['#5a5a5e', '#ff8a80', '#c2e89a', '#ffe566', '#7dbbff', '#ddb3ff', '#9adcf9', '#ffffff'];
const ESC = '\x1b';

/** Le premier segment de la première ligne. @param {string} text */
const first = (text) => parseSgr(text)[0][0];

describe('parseSgr', () => {
  test('reads the sixteen colours at the values of the IdemA palette, foreground and background', () => {
    for (let i = 0; i < 8; i++) {
      expect(first(`${ESC}[${30 + i}mx`).fg).toBe(NORMAL[i]);
      expect(first(`${ESC}[${90 + i}mx`).fg).toBe(BRIGHT[i]);
      expect(first(`${ESC}[${40 + i}mx`).bg).toBe(NORMAL[i]);
      expect(first(`${ESC}[${100 + i}mx`).bg).toBe(BRIGHT[i]);
    }
  });

  test('reads 256 colours: the palette, the cube and the grey ramp', () => {
    expect(first(`${ESC}[38;5;9mx`).fg).toBe(BRIGHT[1]);
    expect(first(`${ESC}[38;5;196mx`).fg).toBe('#ff0000');
    expect(first(`${ESC}[48;5;21mx`).bg).toBe('#0000ff');
    expect(first(`${ESC}[38;5;232mx`).fg).toBe('#080808');
    expect(first(`${ESC}[38;5;255mx`).fg).toBe('#eeeeee');
  });

  test('reads 24-bit colours and clamps what is out of range', () => {
    expect(first(`${ESC}[38;2;1;2;3mx`).fg).toBe('#010203');
    expect(first(`${ESC}[48;2;255;128;0mx`).bg).toBe('#ff8000');
    expect(first(`${ESC}[38;2;300;0;0mx`).fg).toBe('#ff0000');
  });

  test('reads the attributes and their resets', () => {
    const all = first(`${ESC}[1;2;3;4;7mx`);
    expect(all).toMatchObject({ bold: true, dim: true, italic: true, underline: true, inverse: true });
    const [line] = parseSgr(`${ESC}[1;31ma${ESC}[22mb${ESC}[39mc${ESC}[0md`);
    expect(line.map((s) => [s.text, s.bold, s.fg])).toEqual([
      ['a', true, '#ff6961'],
      ['b', false, '#ff6961'],
      ['cd', false, null],
    ]);
    expect(line[2]).toEqual({ text: 'cd', fg: null, bg: null, bold: false, dim: false, italic: false, underline: false, inverse: false });
  });

  test('keeps the style across lines and gives an empty line no segment', () => {
    const lines = parseSgr(`${ESC}[32mone\n\ntwo`);
    expect(lines.length).toBe(3);
    expect(lines[1]).toEqual([]);
    expect(lines[2][0]).toMatchObject({ text: 'two', fg: '#a8d672' });
  });

  test('merges neighbours of the same style', () => {
    expect(parseSgr('abc')[0]).toHaveLength(1);
    expect(parseSgr(`a${ESC}[0mb`)[0]).toHaveLength(1);
  });

  test('a truncated sequence at the end and an unknown code lose no text', () => {
    expect(parseSgr(`before${ESC}[3`)[0].map((s) => s.text).join('')).toBe('before');
    expect(parseSgr(`a${ESC}[99mb`)[0].map((s) => s.text).join('')).toBe('ab');
    // Le code inconnu n'empêche pas les autres du même ordre d'agir.
    expect(first(`${ESC}[1;99;31mx`)).toMatchObject({ bold: true, fg: '#ff6961' });
    // 38 sans la suite : ignoré, le texte reste.
    expect(parseSgr(`${ESC}[38;5mtext`)[0].map((s) => s.text).join('')).toBe('text');
    expect(parseSgr(`${ESC}[38;2;1mtext`)[0].map((s) => s.text).join('')).toBe('text');
  });

  test('drops other escape sequences and control bytes, never the text around them', () => {
    const hostile = `a${ESC}[2Jb${ESC}[?25lc${ESC}]0;title\x07d${ESC}(Be${ESC}cf\x00\x07\x08g\r`;
    expect(parseSgr(hostile)[0].map((s) => s.text).join('')).toBe('abcdefg');
  });

  test('a line of 300 columns and a markup string are plain text', () => {
    const wide = 'x'.repeat(300);
    expect(parseSgr(wide)[0][0].text).toBe(wide);
    expect(parseSgr('<img src=x onerror=alert(1)>')[0][0].text).toBe('<img src=x onerror=alert(1)>');
  });
});

describe('parseSgr, tmux 3.8 forms', () => {
  test('consumes the underline colour parameters like 38 and 48, and ignores 59', () => {
    const fg = (/** @type {string} */ params) => first(`${ESC}[${params}mx`);
    expect(fg('58;5;196;31')).toMatchObject({ fg: '#ff6961', underline: false });
    expect(fg('58;2;1;2;3;1')).toMatchObject({ bold: true, dim: false, italic: false, fg: null });
    // Les paramètres de 58 ne se lisent pas comme des attributs (1 = gras, 2 = atténué, 3 = italique).
    expect(fg('58;5;1')).toMatchObject({ bold: false, dim: false });
    expect(fg('4;58;5;9;32')).toMatchObject({ underline: true, fg: '#a8d672' });
    expect(fg('59;31')).toMatchObject({ fg: '#ff6961' });
    // Aucune couleur de soulignement ne devient une couleur de texte ou de fond.
    expect(fg('58;5;196')).toMatchObject({ fg: null, bg: null });
  });

  test('the colon forms of the underline: 4:n underlines, 4:0 removes it', () => {
    expect(first(`${ESC}[4:3mx`).underline).toBe(true);
    expect(first(`${ESC}[4:1;31mx`)).toMatchObject({ underline: true, fg: '#ff6961' });
    expect(first(`${ESC}[4;4:0mx`).underline).toBe(false);
    expect(first(`${ESC}[4:0mx`).underline).toBe(false);
    expect(parseSgr(`a${ESC}[4:3mb`)[0].map((s) => s.text).join('')).toBe('ab');
  });

  test('an unterminated OSC does not swallow the following lines', () => {
    const lines = parseSgr(`a${ESC}]8;;http://x\nb\nc`);
    expect(lines.map((l) => l.map((s) => s.text).join(''))).toEqual(['a', 'b', 'c']);
  });
});

describe('renderScreen', () => {
  test('an <img onerror> in the capture stays text', () => {
    const pre = renderScreen(parseSgr(`${ESC}[31m<img src=x onerror=alert(1)>${ESC}[0m\n<script>alert(2)</script>`));
    expect(pre.querySelector('img')).toBeNull();
    expect(pre.querySelector('script')).toBeNull();
    expect(pre.textContent).toBe('<img src=x onerror=alert(1)>\n<script>alert(2)</script>');
  });

  test('colours go through the CSSOM and the attributes through classes, never a style attribute', () => {
    const pre = renderScreen(parseSgr(`${ESC}[1;4;38;2;1;2;3;44mhi${ESC}[0m plain`));
    const span = /** @type {HTMLElement} */ (pre.querySelector('span'));
    expect(span.textContent).toBe('hi');
    expect(span.classList.contains('sgr-b')).toBe(true);
    expect(span.classList.contains('sgr-u')).toBe(true);
    expect(span.style.color).not.toBe('');
    expect(span.style.backgroundColor).not.toBe('');
    // Le texte sans style n'a pas de balise.
    expect(pre.querySelectorAll('span').length).toBe(1);
  });

  test('inverse swaps the colours, defaults included', () => {
    const explicit = /** @type {HTMLElement} */ (renderScreen(parseSgr(`${ESC}[7;31;44mx`)).querySelector('span'));
    const reference = document.createElement('span');
    reference.style.color = '#3aa0ff';
    reference.style.backgroundColor = '#ff6961';
    expect(explicit.style.color).toBe(reference.style.color);
    expect(explicit.style.backgroundColor).toBe(reference.style.backgroundColor);
    const bare = /** @type {HTMLElement} */ (renderScreen(parseSgr(`${ESC}[7mx`)).querySelector('span'));
    expect(bare.style.color).toContain('--screen-bg');
    expect(bare.style.backgroundColor).toContain('--screen-text');
  });

  test('every line ends with a line feed except the last, empty lines included', () => {
    expect(renderScreen(parseSgr('a\n\nb')).textContent).toBe('a\n\nb');
  });
});

describe('renderScreen, rule lines', () => {
  /** @param {string} text */
  const render = (text) => renderScreen(parseSgr(text));

  test('a bottom rule renders as one .term__rule whose run holds every character', () => {
    const pre = render('─'.repeat(329));
    expect(pre.querySelectorAll('.term__rule').length).toBe(1);
    const runs = pre.querySelectorAll('.term__rule-run');
    expect(runs.length).toBe(1);
    expect(runs[0].textContent).toBe('─'.repeat(329));
  });

  test('a top rule keeps its label and its single tail character outside the shrinking run', () => {
    const pre = render('─'.repeat(312) + ' some-label-1234 ' + '─');
    const runs = pre.querySelectorAll('.term__rule-run');
    expect(runs.length).toBe(1);
    expect(runs[0].textContent).toBe('─'.repeat(312));
    expect(pre.querySelectorAll('.term__rule').length).toBe(1);
    expect(pre.textContent).toBe('─'.repeat(312) + ' some-label-1234 ' + '─');
  });

  test('trailing whitespace of a rule line goes into a shrinking run', () => {
    const runs = render('─'.repeat(20) + '   ').querySelectorAll('.term__rule-run');
    expect([...runs].map((r) => r.textContent)).toEqual(['─'.repeat(20), '   ']);
  });

  test('a rule wrapped in colour and dim keeps both on its run', () => {
    const run = /** @type {HTMLElement} */ (render(`${ESC}[2;38;2;1;2;3m${'─'.repeat(40)}${ESC}[0m`).querySelector('.term__rule-run'));
    expect(run.classList.contains('sgr-d')).toBe(true);
    expect(run.style.color).not.toBe('');
    expect(run.textContent).toBe('─'.repeat(40));
  });

  test('lines that are not input-box rules keep the plain rendering', () => {
    const table = `├${'─'.repeat(20)}┼${'─'.repeat(20)}┤`;
    const box = `╭${'─'.repeat(30)}╮`;
    const prose = 'a ──── b';
    const longText = '─'.repeat(30) + ' ' + 'x'.repeat(41);
    for (const line of [table, box, prose, longText]) {
      const pre = render(line);
      expect(pre.querySelector('.term__rule')).toBeNull();
      expect(pre.querySelector('.term__rule-run')).toBeNull();
      expect(pre.textContent).toBe(line);
    }
  });

  test('the text content is the plain text of the input, line feeds around the rule included', () => {
    const plain = ['some prose', '─'.repeat(100) + ' label ' + '─', '', `├${'─'.repeat(20)}┼${'─'.repeat(20)}┤`, 'more prose', '─'.repeat(100)].join('\n');
    const withEscapes = plain.replace('some prose', `${ESC}[1msome prose${ESC}[0m`).replace(' label ', ` ${ESC}[36mlabel${ESC}[0m `);
    const pre = render(withEscapes);
    expect(pre.textContent).toBe(plain);
    expect(pre.querySelectorAll('.term__rule').length).toBe(2);
  });
});
