#!/bin/bash
# Télécharge les icônes Material Symbols Rounded d'Idema (Apache-2.0), à une version épinglée.
# Se relance sans risque : chaque fichier est réécrit, et vérifié avant de passer au suivant.
set -euo pipefail
VERSION=0.48.0
DEST="$(cd "$(dirname "$0")/.." && pwd)/crates/idema/assets/icons"
mkdir -p "$DEST"
# fichier:graisse:nom dans le paquet. `expand_more` et `expand_less` sont des alias de police : leurs SVG s'appellent
# keyboard_arrow_down et keyboard_arrow_up.
ICONS="add:300:add add-400:400:add check:400:check work:300:work more_horiz:300:more_horiz \
more_vert:300:more_vert expand_more:300:keyboard_arrow_down expand_less:300:keyboard_arrow_up chevron_right:300:chevron_right \
left_panel_close:300:left_panel_close left_panel_open:300:left_panel_open fork_right:300:fork_right \
terminal:300:terminal open_in_full:300:open_in_full close_fullscreen:300:close_fullscreen close:300:close zoom_in_map:300:zoom_in_map folder:300:folder draft:300:draft description:300:description search:300:search \
match_case:300:match_case match_word:300:match_word regular_expression:300:regular_expression difference:300:difference \
edit_note:300:edit_note stop:400:stop language:300:language public:300:public home:300:home \
arrow_back:300:arrow_back arrow_forward:300:arrow_forward refresh:300:refresh lock_open:300:lock_open \
play_arrow:300:play_arrow arrow_outward:300:arrow_outward sticky_note_2:300:sticky_note_2 \
tune:300:tune smart_toy:300:smart_toy hub:300:hub extension:300:extension storefront:300:storefront \
delete:300:delete visibility:300:visibility visibility_off:300:visibility_off \
filter_list:300:filter_list grid_view:300:grid_view view_list:300:view_list \
filter_none:300:filter_none stacks:300:stacks"
for spec in $ICONS; do
  IFS=: read -r file weight name <<< "$spec"
  out="$DEST/$file.svg"
  for base in https://cdn.jsdelivr.net/npm https://unpkg.com; do
    if curl -sfL -m 30 -o "$out" "$base/@material-symbols/svg-$weight@$VERSION/rounded/$name.svg" \
      && head -c 4 "$out" | grep -q '<svg'; then
      break
    fi
  done
  head -c 4 "$out" | grep -q '<svg' || { echo "icône introuvable : $file ($name, $weight)" >&2; exit 1; }
done
ls "$DEST"
