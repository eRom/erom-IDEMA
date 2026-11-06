#!/bin/bash
# Enregistrement des réponses réelles du service (tâche 16, AC-20), à jouer dans la VM par record.sh :
#   idema-run-ac.sh ../../crates/idema/companion-web/test/visual/vm-record.sh
# Une instance jetable avec le compagnon allumé, trois projets de démonstration (noms de test seulement), cinq sessions
# dans les cinq états que l'app sait montrer : deux attendent (un menu numéroté à la manière de Claude Code, une question
# de Codex), une tourne, une est en erreur, une est au repos. Les réponses de `fleet`, `screen` (de chaque pane),
# `timeline`, `scratchpad`, `me` et `health` sont écrites telles que le service les rend dans out/t16-fixtures/ ;
# record.sh les nettoie ensuite (nom du Mac, chemins) avant de les ranger dans fixtures/.
# Jamais le socket `idema` ni ~/.config/idema. Dernière ligne `T16-RECORD PASS` ou `T16-RECORD FAIL` (sortie non nulle).
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ACC="$HERE/../../../../../tests/acceptance"
source "$ACC/gate-lib.sh"
source "$ACC/companion-lib.sh"
OUT="$ROOT/tests/acceptance/out"
FIX="$OUT/t16-fixtures"
mkdir -p "$OUT" "$GATE_DIR" "$FIX"
exec > >(tee "$OUT/t16-record.log") 2>&1

RESULT=PASS
check() { if eval "$2"; then echo "  ok   : $1"; else echo "  FAIL : $1"; RESULT=FAIL; fi; }
cleanup() {
  [ -n "${TAG_MAIN:-}" ] && [ -f "$GATE_DIR/$TAG_MAIN.pid" ] && "$ACC/stop.sh" "$TAG_MAIN" >/dev/null 2>&1
  return 0
}
trap cleanup EXIT
wait_until() {
  local limit="$1" i; shift
  for i in $(seq 1 $((limit * 10))); do if "$@"; then return 0; fi; sleep 0.1; done
  return 1
}
fleet_is() { cget /idema-api/fleet; [ "$CCODE" = 200 ] && printf '%s' "$CBODY" | jq -e "$1" >/dev/null 2>&1; }
state_of() { printf '(.sessions[] | select(.display_name == "%s") | .state == "%s")' "$1" "$2"; }

TAG_MAIN="t16-$$"
BASE="$GATE_DIR/demo-$TAG_MAIN"
P_IDEMA="$BASE/idema-demo"; P_BLOG="$BASE/blog-demo"; P_NOTES="$BASE/notes-demo"
# Deux liens vers le faux agent, nommés comme les vrais : la ligne de commande vue à l'écran est « claude » ou « codex ».
BIN="$BASE/bin"
mkdir -p "$BIN"
ln -sf "$HERE/menu-agent.sh" "$BIN/claude"
ln -sf "$HERE/menu-agent.sh" "$BIN/codex"
TOML=$(printf 'sound_on_wait = false\n\n[companion]\nport = %s\n\n[[agents]]\nname = "Claude"\ncommand = "%s/claude"\n\n[[agents]]\nname = "Codex"\ncommand = "%s/codex"\n' "$CPORT" "$BIN" "$BIN")

echo "=== préparation"
companion_launch "$TAG_MAIN" "$TOML" "$P_IDEMA" "$P_BLOG" "$P_NOTES"
check "l'amorçage d'IdemA est fini" 'wait_ready'
check "l'appairage par la route de test réussit" 'companion_pair'

echo "=== cinq sessions, cinq états"
read -r S1 P1 < <("$ACC/new-session.sh" "Refonte du rendu" "$P_IDEMA")
read -r S2 P2 < <("$ACC/new-session.sh" "Migration des données" "$P_IDEMA")
read -r S3 P3 < <("$ACC/new-session.sh" "Tri des notes" "$P_NOTES")
read -r S4 P4 < <("$ACC/new-session.sh" "Correctif des tests" "$P_BLOG")
read -r S5 P5 < <("$ACC/new-session.sh" "Revue de code" "$P_BLOG")
check "les cinq sessions figurent dans la flotte" 'wait_until 10 fleet_is "(.sessions | length) == 5"'
# Le PATH, puis un écran nettoyé : seule la ligne « claude » (ou « codex ») précède la sortie du faux agent.
for pane in "$P1" "$P5"; do t send-keys -t "$pane" "export PATH='$BIN':\$PATH" Enter; done
sleep 0.5
for pane in "$P1" "$P5"; do t send-keys -t "$pane" 'clear' Enter; done
sleep 0.5
# `clear` laisse l'historique : la route d'écran le rend aussi (200 lignes par défaut).
for pane in "$P1" "$P5"; do t clear-history -t "$pane"; done
t send-keys -t "$P1" 'claude' Enter
t send-keys -t "$P2" 'for i in $(seq 1 600); do echo "etape $i : migration du lot"; sleep 1; done' Enter
t send-keys -t "$P4" 'false' Enter
t send-keys -t "$P5" 'codex' Enter
check "Refonte du rendu et Revue de code attendent" 'wait_until 25 fleet_is "$(state_of "Refonte du rendu" waiting) and $(state_of "Revue de code" waiting)"'
check "Migration des données tourne" 'wait_until 15 fleet_is "$(state_of "Migration des données" running)"'
check "Correctif des tests est en erreur" 'wait_until 15 fleet_is "$(state_of "Correctif des tests" error)"'
check "Tri des notes est au repos" 'wait_until 5 fleet_is "$(state_of "Tri des notes" idle)"'
check "Refonte du rendu porte l'agent Claude, et Revue de code l'agent Codex, avec leurs logos" 'wait_until 10 fleet_is "(.sessions[] | select(.display_name == \"Refonte du rendu\") | .agents == [{label: \"Claude\", logo: \"claude\"}]) and (.sessions[] | select(.display_name == \"Revue de code\") | .agents == [{label: \"Codex\", logo: \"codex\"}])"'

echo "=== le Scratchpad"
cpost /idema-api/scratchpad/append '{"line":"Rappeler le plombier avant jeudi"}'
check "la première ligne est ajoutée" '[ "$CCODE" = 200 ]'
cpost /idema-api/scratchpad/append '{"line":"Idée : un raccourci pour acquitter toute la flotte"}'
cpost /idema-api/scratchpad/append '{"line":"Doc du protocole : https://example.com/doc"}'
check "les trois lignes sont ajoutées" '[ "$CCODE" = 200 ]'

echo "=== enregistrement"
sleep 1
record() { cget "$1"; printf '%s' "$CBODY" > "$FIX/$2.json"; check "$2 : 200" '[ "$CCODE" = 200 ]'; }
record /idema-api/fleet fleet
record /idema-api/timeline timeline
record /idema-api/scratchpad scratchpad
record /idema-api/me me
record /idema-api/health health
# Les écrans de chaque pane, rangés par nom affiché puis rang du pane : { "<nom>": { "<rang>": <réponse>, … } }.
printf '{}' > "$FIX/screens.json"
FLEET=$(cat "$FIX/fleet.json")
for name in "Refonte du rendu" "Migration des données" "Tri des notes" "Correctif des tests" "Revue de code"; do
  session=$(printf '%s' "$FLEET" | jq -r --arg n "$name" '.sessions[] | select(.display_name == $n) | .name')
  rank=0
  for pane in $(printf '%s' "$FLEET" | jq -r --arg n "$name" '.sessions[] | select(.display_name == $n) | .panes[].id'); do
    cget "/idema-api/sessions/$session/panes/$pane/screen"
    check "écran de « $name » (pane $pane) : 200" '[ "$CCODE" = 200 ]'
    jq --arg n "$name" --arg r "$rank" --argjson s "$CBODY" '.[$n][$r] = $s' "$FIX/screens.json" > "$FIX/screens.tmp" && mv "$FIX/screens.tmp" "$FIX/screens.json"
    rank=$((rank + 1))
  done
done
check "le menu à choix de Refonte du rendu est dans son écran" 'jq -e ".[\"Refonte du rendu\"][\"0\"] | .accepts_input == true and .state == \"waiting\" and (.text | contains(\"2. Yes, allow all edits\"))" "$FIX/screens.json" >/dev/null'
check "la question de Codex dans Revue de code est dans son écran" 'jq -e ".[\"Revue de code\"][\"0\"] | .accepts_input == true and (.text | contains(\"1) Yes, proceed\"))" "$FIX/screens.json" >/dev/null'
check "la chronologie porte des événements" 'jq -e ".events | length > 0" "$FIX/timeline.json" >/dev/null'
echo "  fixtures : $(ls "$FIX" | tr '\n' ' ')"

echo "T16-RECORD $RESULT"
[ "$RESULT" = PASS ]
