import { describe, expect, test } from 'bun:test';
import { bannerText, createRetry } from '../js/offline.js';

const NOW = 1_800_000_000_000;

describe('bannerText', () => {
  test('an immediate failure points at Tailscale', () => {
    expect(bannerText('offline', null, NOW)).toBe('Mac injoignable : Tailscale est-il actif ?');
  });

  test('a timeout points at sleep or a closed IdemA', () => {
    expect(bannerText('timeout', null, NOW)).toBe('Le Mac ne répond pas : en veille, ou IdemA fermé ?');
  });

  test('the age of the data is appended when there is some', () => {
    expect(bannerText('offline', NOW - 3 * 60_000, NOW)).toBe(
      "Mac injoignable : Tailscale est-il actif ? Données d'il y a 3 min.",
    );
  });
});

describe('createRetry', () => {
  test('with the real timers it ticks until stopped', async () => {
    let runs = 0;
    const retry = createRetry(() => runs++, 10);
    retry.start();
    await new Promise((resolve) => setTimeout(resolve, 80));
    retry.stop();
    const seen = runs;
    expect(seen).toBeGreaterThan(0);
    await new Promise((resolve) => setTimeout(resolve, 40));
    expect(runs).toBe(seen);
  });

  test('runs on the interval while started, never twice at once, and stops', () => {
    /** @type {(() => void)[]} */
    const ticks = [];
    let cleared = 0;
    const retry = createRetry(() => runs++, 5000, {
      setInterval: (fn) => (ticks.push(fn), ticks.length),
      clearInterval: () => cleared++,
    });
    let runs = 0;
    retry.start();
    retry.start();
    expect(ticks.length).toBe(1);
    ticks[0]();
    expect(runs).toBe(1);
    retry.stop();
    retry.stop();
    expect(cleared).toBe(1);
    retry.start();
    expect(ticks.length).toBe(2);
  });
});
