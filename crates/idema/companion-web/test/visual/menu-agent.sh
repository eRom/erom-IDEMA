#!/bin/bash
# Faux agent des captures (tâche 16) : affiche une demande d'autorisation, active le collage entre crochets, puis sonne
# (\a) : la session passe à « attend » avec le menu à l'écran. Le programme se choisit par le nom sous lequel il est
# lancé (un lien `claude` ou `codex` vers ce fichier) : menu numéroté de Claude Code, ou question de Codex.
# Aucun contenu réel : un fichier et une commande de démonstration.
E=$'\033'
printf '\033[?2004h'
if [ "$(basename "$0")" = codex ]; then
  printf '%s\n' \
    '  Would you like to run the following command?' \
    '' \
    "  ${E}[1m\$ cargo test -p blog-demo${E}[0m" \
    '' \
    "${E}[36m›${E}[0m ${E}[36m1) Yes, proceed${E}[0m" \
    "  2) Yes, and don't ask again for this command" \
    '  3) No, and tell Codex what to do differently'
else
  # Un cadre de 50 colonnes : chaque rangée est complétée à la longueur de son texte brut (les couleurs ne comptent pas).
  B="${E}[38;2;197;138;249m"; R="${E}[0m"
  row() { printf '%s│%s %s%*s%s│%s\n' "$B" "$R" "$1" "$((49 - $2))" '' "$B" "$R"; }
  RULE="$(printf '─%.0s' $(seq 1 50))"
  printf '%s\n' \
    "${E}[38;5;208m●${E}[0m ${E}[1mEdit${E}[0m(src/render.rs)" \
    "  ${E}[2m⎿${E}[0m  ${E}[32m+ let rows = measure(text)?;${E}[0m" \
    "  ${E}[2m⎿${E}[0m  ${E}[31m- let rows = old_measure(text)?;${E}[0m" \
    "" \
    "${B}╭${RULE}╮${R}"
  row "Do you want to make this edit to render.rs?" 43
  row "${E}[38;2;64;156;255m❯ 1. Yes${R}" 8
  row "  2. Yes, allow all edits during this session" 45
  row "  3. No, and tell Claude what to do differently" 47
  printf '%s\n' "${B}╰${RULE}╯${R}"
fi
sleep 2
printf '\a'
sleep 600
