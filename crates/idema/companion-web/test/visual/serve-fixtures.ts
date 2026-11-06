// Serveur des captures (tâche 16, AC-20) : les fichiers de l'app, servis avec les en-têtes exacts du service du Mac
// (la CSP lue dans server.rs, `nosniff`, gzip précompressé avec son propre ETag et `Vary`), et l'API rejouée depuis des
// réponses enregistrées dans la VM (fixtures/). Sans les mêmes en-têtes, les captures jugeraient un rendu que la CSP
// n'a jamais contraint.
//
// Usage : bun serve-fixtures.ts [--port 3999] [--fixtures <dossier>] [--no-gzip]
// Le mode se change par GET /__mode/<paired|unpaired|offline|down> :
//   paired    l'appareil est appairé, l'API rejoue les fixtures ;
//   unpaired  l'API rend 401 `unpaired` (sauf `health` et `pair`) : l'écran d'appairage ;
//   offline   l'API rend 503, les fichiers sont servis : le bandeau « hors ligne » sur la flotte en cache ;
//   down      tout rend 503, fichiers compris : seule la copie du service worker répond.
// GET /__quit arrête le serveur. Les instants des fixtures sont ramenés à maintenant (la flotte enregistrée il y a
// des jours se lit « il y a 3 min »), en gardant les écarts entre eux.

import { existsSync, readFileSync } from 'node:fs';
import { join, resolve } from 'node:path';

const HERE = import.meta.dir;
const APP = resolve(HERE, '../..');
const LOGOS = resolve(HERE, '../../../assets/logos');
const SERVER_RS = resolve(HERE, '../../../src/companion/server.rs');

const args = process.argv.slice(2);
const option = (name: string, fallback: string) => {
  const at = args.indexOf(name);
  return at >= 0 && args[at + 1] ? args[at + 1] : fallback;
};
const PORT = Number(option('--port', '3999'));
const FIXTURES = resolve(option('--fixtures', join(HERE, 'fixtures')));
const GZIP = !args.includes('--no-gzip');

/** La CSP du service, lue à sa source : elle ne peut pas diverger de celle qu'on juge. */
function readCsp(): string {
  const match = /const CONTENT_SECURITY_POLICY: &str = "([^"]+)";/.exec(readFileSync(SERVER_RS, 'utf8'));
  if (!match) throw new Error(`CONTENT_SECURITY_POLICY introuvable dans ${SERVER_RS}`);
  return match[1];
}
const CSP = readCsp();

const TYPES: Record<string, string> = {
  html: 'text/html; charset=utf-8',
  css: 'text/css; charset=utf-8',
  js: 'text/javascript; charset=utf-8',
  webmanifest: 'application/manifest+json',
  png: 'image/png',
};

const fixture = (name: string) => JSON.parse(readFileSync(join(FIXTURES, `${name}.json`), 'utf8'));
const FLEET = fixture('fleet');
const TIMELINE = fixture('timeline');
const SCRATCHPAD = fixture('scratchpad');
const ME = fixture('me');
const HEALTH = fixture('health');
const SCREENS: Record<string, Record<string, any>> = fixture('screens');

/** Décalage qui ramène l'instant de l'enregistrement à maintenant. */
const SHIFT = Date.now() - FLEET.generated_at;
const shifted = (at: number) => at + SHIFT;

function fleetNow() {
  return {
    ...FLEET,
    generated_at: shifted(FLEET.generated_at),
    sessions: FLEET.sessions.map((s: any) => ({ ...s, state_since: shifted(s.state_since) })),
  };
}
function timelineNow() {
  return {
    started_at: shifted(TIMELINE.started_at),
    events: TIMELINE.events.map((e: any) => ({ ...e, at: shifted(e.at) })),
  };
}
let note: string = SCRATCHPAD.text;

type Mode = 'paired' | 'unpaired' | 'offline' | 'down';
let mode: Mode = 'paired';

function secure(headers: Record<string, string> = {}): Record<string, string> {
  return { 'content-security-policy': CSP, 'x-content-type-options': 'nosniff', 'cache-control': 'no-cache', ...headers };
}
function json(status: number, body: unknown, extra: Record<string, string> = {}) {
  return new Response(JSON.stringify(body), { status, headers: secure({ 'content-type': 'application/json', ...extra }) });
}
const plain = (status: number) => new Response(null, { status, headers: secure() });

/** Un fichier de l'app, avec son ETag, le gzip quand le client l'accepte, et Vary, comme web_assets.rs et server.rs. */
const zipped = new Map<string, Uint8Array>();
function appFile(path: string, request: Request): Response {
  const file = path === '/' ? join(APP, 'index.html') : join(APP, path);
  const logo = path.startsWith('/logos/') ? join(LOGOS, path.slice('/logos/'.length)) : null;
  const disk = logo ?? file;
  // Le dossier de l'app seulement : aucun chemin ne sort de APP ou de LOGOS.
  const inside = logo ? disk.startsWith(LOGOS + '/') : disk.startsWith(APP + '/');
  if (!inside || path.includes('..') || !existsSync(disk) || disk.endsWith('/')) return plain(404);
  const extension = disk.split('.').pop() ?? '';
  const type = TYPES[extension];
  if (!type) return plain(404);
  const bytes = new Uint8Array(readFileSync(disk));
  const etag = `"${Bun.hash(bytes).toString(16)}"`;
  const textual = extension !== 'png';
  const wantsGzip = GZIP && textual && /\bgzip\b|\*/.test(request.headers.get('accept-encoding') ?? '');
  const sent = wantsGzip ? `${etag.slice(0, -1)}-gz"` : etag;
  const vary: Record<string, string> = textual ? { vary: 'Accept-Encoding' } : {};
  const none = request.headers.get('if-none-match');
  if (none && none.split(',').some((candidate) => candidate.trim().replace(/^W\//, '') === sent)) {
    return new Response(null, { status: 304, headers: secure({ etag: sent, ...vary }) });
  }
  let body: Uint8Array = bytes;
  if (wantsGzip) {
    if (!zipped.has(path)) zipped.set(path, Bun.gzipSync(bytes, { level: 9 }));
    body = zipped.get(path)!;
  }
  const encoding: Record<string, string> = wantsGzip ? { 'content-encoding': 'gzip' } : {};
  const headers = secure({ 'content-type': type, etag: sent, ...vary, ...encoding });
  return new Response(request.method === 'HEAD' ? null : (body as BodyInit), { status: 200, headers });
}

/** L'écran d'un pane : rang du pane dans sa session, puis réponse enregistrée. */
function screen(sessionName: string, pane: number, request: Request): Response {
  const session = FLEET.sessions.find((s: any) => s.name === sessionName);
  const rank = session?.panes.findIndex((p: any) => p.id === pane) ?? -1;
  const recorded = session && rank >= 0 ? SCREENS[session.display_name]?.[String(rank)] : undefined;
  if (!recorded) return json(404, { error: 'not_found' });
  const etag = `"${Bun.hash(`${recorded.text}|${recorded.state}|${recorded.accepts_input}`).toString(16)}"`;
  if (request.headers.get('if-none-match') === etag) return new Response(null, { status: 304, headers: secure({ etag, vary: 'Accept-Encoding' }) });
  return json(200, recorded, { etag, vary: 'Accept-Encoding' });
}

function events(): Response {
  const encoder = new TextEncoder();
  const frame = `event: fleet\ndata: ${JSON.stringify(fleetNow())}\n\n`;
  const stream = new ReadableStream({
    start(controller) {
      controller.enqueue(encoder.encode(frame));
    },
  });
  return new Response(stream, { headers: secure({ 'content-type': 'text/event-stream' }) });
}

async function api(route: string, request: Request): Promise<Response> {
  if (route === '/health') return json(200, HEALTH);
  if (route === '/pair' && request.method === 'POST') {
    const body = (await request.json().catch(() => ({}))) as { code?: string };
    if (body.code === '123456') {
      mode = 'paired';
      return json(200, { device: ME.device });
    }
    return json(403, { error: 'wrong_code' });
  }
  if (mode === 'offline') return json(503, { error: 'unavailable' });
  if (mode === 'unpaired') return json(401, { error: 'unpaired' });
  const pane = /^\/sessions\/([^/]+)\/panes\/(\d+)\/(screen|input|ack)$/.exec(route);
  if (pane) {
    if (pane[3] === 'screen') return screen(decodeURIComponent(pane[1]), Number(pane[2]), request);
    return json(200, pane[3] === 'input' ? { ok: true, outcome: 'sent' } : { ok: true });
  }
  if (route === '/me') return json(200, ME);
  if (route === '/fleet') return json(200, fleetNow());
  if (route === '/timeline') return json(200, timelineNow());
  if (route === '/events') return events();
  if (route === '/scratchpad') return json(200, { text: note, modified_at: Date.now() - 5 * 60_000 });
  if (route === '/scratchpad/append' && request.method === 'POST') {
    const { line } = (await request.json()) as { line: string };
    note += `\n${line}`;
    return json(200, { ok: true });
  }
  if (route === '/push/key') return json(200, { key: 'BEl62iUYgUivxIkv69yViEuiBIa-Ib9-SkvMeAtA3LFgDzkrxZJjSgSnfckjBJuBkr3qBUYIHBQFLXYp5Nksh8U' });
  return json(200, { ok: true });
}

const server = Bun.serve({
  port: PORT,
  hostname: '127.0.0.1',
  async fetch(request) {
    const { pathname } = new URL(request.url);
    if (pathname === '/__quit') {
      setTimeout(() => process.exit(0), 50);
      return json(200, { bye: true });
    }
    const wanted = /^\/__mode\/(paired|unpaired|offline|down)$/.exec(pathname);
    if (wanted) {
      mode = wanted[1] as Mode;
      return json(200, { mode });
    }
    if (mode === 'down') return plain(503);
    if (pathname.startsWith('/idema-api/')) return api(pathname.slice('/idema-api'.length), request);
    if (request.method !== 'GET' && request.method !== 'HEAD') return plain(405);
    return appFile(pathname, request);
  },
});
console.log(`listening ${server.hostname}:${server.port} fixtures=${FIXTURES} gzip=${GZIP} csp="${CSP}"`);
