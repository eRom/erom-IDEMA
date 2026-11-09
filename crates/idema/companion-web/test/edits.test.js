import { expect, test } from 'bun:test';
import { addDash, indent, insertFence, insertHash, outdent } from '../js/edits.js';

/** @param {string} text @param {number} start @param {number} [end] */
const st =(text, start, end = start) => ({ text, start, end });

test('# inserts the character at the caret, replacing the selection', () => {
  expect(insertHash(st('ab', 1))).toEqual(st('a#b', 2));
  expect(insertHash(st('abc', 1, 2))).toEqual(st('a#c', 2));
});
test('- prefixes the current line after its indent, once', () => {
  expect(addDash(st('x\nitem', 4))).toEqual(st('x\n- item', 6));
  expect(addDash(st('  item', 3))).toEqual(st('  - item', 5));
  expect(addDash(st('- item', 3))).toEqual(st('- item', 3));
  expect(addDash(st('', 0))).toEqual(st('- ', 2));
});
test('⇥ adds two spaces to every line the selection touches', () => {
  expect(indent(st('a\nb', 3))).toEqual(st('a\n  b', 5));
  expect(indent(st('a\nb\nc', 0, 3))).toEqual(st('  a\n  b\nc', 2, 7));
  expect(indent(st('a\nb\n', 0, 4))).toEqual(st('  a\n  b\n', 2, 8));
});
test('⇤ removes up to two leading spaces from every touched line', () => {
  expect(outdent(st('    a', 4))).toEqual(st('  a', 2));
  expect(outdent(st(' a', 0))).toEqual(st('a', 0));
  expect(outdent(st('a', 1))).toEqual(st('a', 1));
  expect(outdent(st('  a\n  b', 2, 7))).toEqual(st('a\nb', 0, 3));
});
test('``` inserts a fenced block on its own lines with the caret inside', () => {
  expect(insertFence(st('', 0))).toEqual(st('```\n\n```', 4));
  expect(insertFence(st('ab', 2))).toEqual(st('ab\n```\n\n```', 7));
  expect(insertFence(st('a\n\nb', 2))).toEqual(st('a\n```\n\n```\nb', 6));
  expect(insertFence(st('ab', 0))).toEqual(st('```\n\n```\nab', 4));
});
