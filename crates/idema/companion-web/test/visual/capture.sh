#!/bin/bash
# Les sept écrans de l'app dans la fenêtre d'un iPhone 15 Pro Max (tâche 16, AC-20) : WebKit sans interface par
# playwright-cli, contre serve-fixtures.ts (les réponses enregistrées dans la VM, les en-têtes du service, CSP comprise).
# À lancer depuis le dossier de travail (scratchpad) : playwright-cli y écrit `.playwright-cli/`, jamais dans le dépôt.
# Usage : capture.sh <dossier de sortie> [préfixe, « t16- » par défaut]
# Sortie : <préfixe>{install,pairing,fleet,session,absence,notes,offline}.png (1290 x 2796, 3x) et, à côté, leur
# <écran>.points.json (les points que colors.py échantillonne). Sort en non-zéro si une ligne FAIL est imprimée.
# Hors ligne : le serveur rend 503 partout, fichiers compris. La page ne vit plus que de la copie du service worker, ce qui
# vérifie aussi qu'une réponse gzip mise en cache est rejouée telle quelle (ni double décompression, ni style perdu).
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
if git rev-parse --show-toplevel >/dev/null 2>&1; then
  echo "capture: run me from a scratch folder outside any git repo (playwright-cli writes .playwright-cli/ here)" >&2
  exit 2
fi
OUT="${1:?usage: capture.sh <output folder> [prefix]}"
PREFIX="${2:-t16-}"
mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"
PORT="${CAPTURE_PORT:-3999}"
BASE="http://127.0.0.1:$PORT"
SESSION="t16"
CONFIG="$HERE/playwright.config.json"
# playwright-cli ne lit un fichier de code que sous le dossier courant.
TMPJS="$PWD/.t16-shoot-$$.js"

RESULT=PASS
check() { if eval "$2"; then echo "  ok   : $1"; else echo "  FAIL : $1"; RESULT=FAIL; fi; }
pw() { playwright-cli -s="$SESSION" "$@"; }
# Une erreur de page fait échouer l'étape : playwright-cli imprime « ### Error » sans toujours sortir en non-zéro.
code() {
  local out
  out="$(pw run-code "$1" 2>&1)"
  if printf '%s' "$out" | grep -q '^### Error'; then printf '%s\n' "$out" >&2; return 1; fi
  printf '%s\n' "$out"
}
wait_for() { code "async (page) => { await page.waitForSelector('$1', { timeout: 15000 }); }" >/dev/null; }
hash_to() { pw eval "() => { location.hash = '$1'; }" >/dev/null; }

SERVER_PID=""
cleanup() {
  pw close >/dev/null 2>&1
  if [ -n "$SERVER_PID" ]; then kill "$SERVER_PID" 2>/dev/null; wait "$SERVER_PID" 2>/dev/null; fi
  [ -f "$TMPJS" ] && trash "$TMPJS"
  return 0
}
trap cleanup EXIT

shoot() {
  sed -e "s#__OUT__#$OUT#g" -e "s#__NAME__#$1#g" -e "s#__PREFIX__#$PREFIX#g" "$HERE/shoot.js" > "$TMPJS"
  local raw
  raw="$(pw --raw run-code --filename "$TMPJS" 2>&1)"
  if ! printf '%s' "$raw" | jq -e 'if type == "string" then fromjson else . end | has("points")' >/dev/null 2>&1; then
    echo "  FAIL : capture of $1: $raw"; RESULT=FAIL; return
  fi
  local report
  report="$(printf '%s' "$raw" | jq 'if type == "string" then fromjson else . end')"
  printf '%s' "$report" | jq .points > "$OUT/${PREFIX}$1.points.json"
  check "$1 captured to ${PREFIX}$1.png" "[ -s '$OUT/${PREFIX}$1.png' ]"
  printf '%s' "$report" | jq -c '{csp, errors}' > "$OUT/${PREFIX}$1.report.json"
  check "$1 : aucune violation de la CSP, aucune erreur de page" "jq -e '(.csp | length) == 0 and (.errors | length) == 0' '$OUT/${PREFIX}$1.report.json' >/dev/null"
}

echo "=== serveur des réponses enregistrées"
bun "$HERE/serve-fixtures.ts" --port "$PORT" > "$OUT/${PREFIX}server.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 50); do curl -s -o /dev/null "$BASE/idema-api/health" && break; sleep 0.1; done
check "le serveur répond sur $BASE" 'curl -s -f -o /dev/null "$BASE/idema-api/health"'
HEAD="$(curl -s -D - -o /dev/null -H 'Accept-Encoding: gzip' "$BASE/js/main.js" | tr -d '\r')"
check "les fichiers sont servis en gzip avec la CSP, nosniff et Vary" \
  'printf "%s" "$HEAD" | grep -qi "^content-encoding: gzip" && printf "%s" "$HEAD" | grep -qi "^content-security-policy: default-src" && printf "%s" "$HEAD" | grep -qi "^x-content-type-options: nosniff" && printf "%s" "$HEAD" | grep -qi "^vary: accept-encoding"'

echo "=== navigateur : WebKit, 430 x 932 à 3x, tactile"
pw close >/dev/null 2>&1
pw open --browser=webkit --config="$CONFIG" "$BASE/" >/dev/null
# Les violations de CSP et les erreurs de page, relevées dès le chargement de chaque document, relues par shoot.js.
code "async (page) => {
  await page.context().addInitScript(() => {
    window.__csp = [];
    window.__errors = [];
    document.addEventListener('securitypolicyviolation', (e) => window.__csp.push(e.violatedDirective + ' ' + e.blockedURI));
    addEventListener('error', (e) => window.__errors.push(String(e.message)));
    addEventListener('unhandledrejection', (e) => window.__errors.push(String(e.reason)));
  });
}" >/dev/null

echo "=== Installation : dans un onglet de Safari, hors de l'écran d'accueil"
pw goto "$BASE/" >/dev/null
check "l'écran d'installation s'affiche" 'wait_for ".steps"'
shoot install

echo "=== Appairage : app installée, appareil inconnu du Mac"
curl -s "$BASE/__mode/unpaired" >/dev/null
code "async (page) => { await page.context().addInitScript(() => Object.defineProperty(navigator, 'standalone', { value: true })); await page.evaluate(() => localStorage.clear()); }" >/dev/null
pw goto "$BASE/" >/dev/null
check "l'écran d'appairage s'affiche" 'wait_for ".code"'
shoot pairing

echo "=== Flotte : après l'appairage"
curl -s "$BASE/__mode/paired" >/dev/null
code "async (page) => { await page.locator('.code__input').fill('123456'); }" >/dev/null
check "la flotte s'affiche (au moins une session)" 'wait_for ".row"'
check "les pastilles d'attente, d'exécution et d'erreur sont là" 'wait_for ".dot--waiting" && wait_for ".dot--running" && wait_for ".dot--error"'
shoot fleet

echo "=== Session : le menu à choix lu"
SESSION_NAME="$(jq -r '.sessions[] | select(.display_name == "Refonte du rendu") | .name' "$HERE/fixtures/fleet.json")"
hash_to "#/s/$SESSION_NAME/0"
check "les choix du menu sont lus" 'wait_for ".choices button"'
shoot session

echo "=== Absence : la dernière visite remonte à une heure"
# Le rechargement fait masquer la page, donc réécrit la dernière visite : elle se pose à chaque chargement du document.
code "async (page) => { await page.context().addInitScript(() => localStorage.setItem('idema.lastVisit', String(Date.now() - 3600000))); }" >/dev/null
pw reload >/dev/null
hash_to "#/absence"
check "l'absence s'affiche" 'wait_for ".row"'
shoot absence

echo "=== Notes"
hash_to "#/notes"
check "la note s'affiche" 'wait_for ".notes__text"'
shoot notes

echo "=== Hors ligne : le Mac ne répond plus, la page vit de la copie du service worker"
hash_to "#/"
wait_for ".row" >/dev/null
# Le service worker prend le contrôle au rechargement qui suit son installation ; on attend que son cache soit complet.
CACHED="$(code "async (page) => { return await page.evaluate(async () => {
  await navigator.serviceWorker.ready;
  for (let i = 0; i < 60; i++) {
    const names = (await caches.keys()).filter((n) => n.startsWith('idema-'));
    for (const name of names) {
      const cache = await caches.open(name);
      if (await cache.match('/')) return 'ok ' + name + ' ' + (await cache.keys()).length;
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  return 'no complete cache';
}); }")"
echo "  cache : $(printf '%s' "$CACHED" | tr '\n' ' ' | cut -c1-120)"
check "le service worker a mis l'app en cache" 'printf "%s" "$CACHED" | grep -q "ok idema-"'
pw reload >/dev/null
wait_for ".row" >/dev/null
curl -s "$BASE/__mode/down" >/dev/null
check "le serveur rend 503 partout" '[ "$(curl -s -o /dev/null -w "%{http_code}" "$BASE/")" = 503 ]'
pw reload >/dev/null
check "la page se rouvre sans le Mac (fichiers rejoués depuis le cache)" 'wait_for ".row"'
check "le bandeau dit que le Mac ne répond pas" 'wait_for ".banner:not([hidden])"'
REPLAY="$(code "async (page) => { return await page.evaluate(() => JSON.stringify({
  controlled: !!navigator.serviceWorker.controller,
  rows: document.querySelectorAll('.row').length,
  background: getComputedStyle(document.body).backgroundColor,
  banner: document.querySelector('.banner').textContent,
  scripts: [...document.scripts].length,
})); }")"
echo "  rejeu : $(printf '%s' "$REPLAY" | tr '\n' ' ' | cut -c1-300)"
check "le service worker contrôle la page, la flotte du cache est là, la feuille de style a été rejouée" \
  'printf "%s" "$REPLAY" | grep -q "\"controlled\\\\\":true" && printf "%s" "$REPLAY" | grep -q "rgb(21, 21, 22)" && ! printf "%s" "$REPLAY" | grep -q "\\\\\"rows\\\\\":0"'
shoot offline
curl -s "$BASE/__mode/paired" >/dev/null

echo "=== Écritures : aucune ne doit être refusée en 415 comme le ferait le garde du Mac"
check "toutes les écritures de la page portent un corps JSON (aucun 415)" 'curl -s "$BASE/__rejected" | jq -e ".rejected | length == 0" >/dev/null'
check "l'acquittement du pane est bien parti (la vérification ci-dessus ne porte pas sur du vide)" 'curl -s "$BASE/__rejected" | jq -e "[.accepted[] | select(endswith(\"/ack\"))] | length >= 1" >/dev/null'

echo "capture $RESULT"
[ "$RESULT" = PASS ]
