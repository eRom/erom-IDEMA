import { describe, expect, test } from 'bun:test';
import { detectChoices } from '../js/choices.js';

describe('detectChoices', () => {
  test('reads a Claude Code menu, the selected line marked by the arrow', () => {
    const lines = [
      '  Edit file src/main.rs',
      '',
      ' Do you want to make this edit?',
      ' ❯ 1. Yes',
      '   2. Yes, allow all edits during this session (shift+tab)',
      '   3. No, and tell Claude what to do differently (esc)',
      '',
      '',
    ];
    expect(detectChoices(lines)).toEqual({
      kind: 'menu',
      options: [
        { n: 1, label: 'Yes', selected: true },
        { n: 2, label: 'Yes, allow all edits during this session (shift+tab)', selected: false },
        { n: 3, label: 'No, and tell Claude what to do differently (esc)', selected: false },
      ],
    });
  });

  test('reads a Claude Code menu drawn inside a box', () => {
    const lines = ['│ Do you want to proceed?        │', '│ ❯ 1. Yes                       │', '│   2. No, keep planning         │'];
    const found = detectChoices(lines);
    expect(found).toEqual({
      kind: 'menu',
      options: [
        { n: 1, label: 'Yes', selected: true },
        { n: 2, label: 'No, keep planning', selected: false },
      ],
    });
  });

  test('reads a Codex menu with the parenthesis form and its own marker', () => {
    const lines = ['  Would you like to run the following command?', '  $ ls -la', '› 1) Yes, proceed', '  2) No, and tell Codex what to do differently'];
    expect(detectChoices(lines)).toEqual({
      kind: 'menu',
      options: [
        { n: 1, label: 'Yes, proceed', selected: true },
        { n: 2, label: 'No, and tell Codex what to do differently', selected: false },
      ],
    });
  });

  test('a documentation list with a single entry is not a menu', () => {
    expect(detectChoices(['Steps:', '1. install the thing', 'then run it'])).toBeNull();
  });

  test('a numbering that skips, or does not start at 1, is not a menu', () => {
    expect(detectChoices(['1. one', '3. three'])).toBeNull();
    expect(detectChoices(['2. two', '3. three'])).toBeNull();
    expect(detectChoices(['1. one', '2. two', '4. four'])).toBeNull();
  });

  test('more than nine entries is not a menu', () => {
    const lines = Array.from({ length: 10 }, (_, i) => `${i + 1}. entry ${i + 1}`);
    expect(detectChoices(lines)).toBeNull();
  });

  test('only the last twenty lines count, and the last numbering wins', () => {
    const old = ['1. old one', '2. old two'];
    const filler = Array.from({ length: 25 }, () => 'text');
    expect(detectChoices([...old, ...filler])).toBeNull();
    const menu = detectChoices(['1. earlier', '2. list', 'text', '1. Apply', '2. Show the diff first']);
    expect(menu).toMatchObject({ kind: 'menu', options: [{ n: 1, label: 'Apply' }, { n: 2, label: 'Show the diff first' }] });
  });

  test('reads a yes/no prompt in the last three lines, in its usual spellings', () => {
    expect(detectChoices(['Overwrite the file? [y/N]'])).toEqual({ kind: 'yesno' });
    expect(detectChoices(['Continue? (y/n)'])).toEqual({ kind: 'yesno' });
    expect(detectChoices(['Proceed? [Y/n]', '', ''])).toEqual({ kind: 'yesno' });
    expect(detectChoices(['Type yes/no to confirm'])).toEqual({ kind: 'yesno' });
    expect(detectChoices(['Continue? (y/n)', 'a', 'b', 'c', 'd'])).toBeNull();
  });

  test('a screen without a prompt gives nothing', () => {
    expect(detectChoices([])).toBeNull();
    expect(detectChoices(['$ ', ''])).toBeNull();
  });
});
