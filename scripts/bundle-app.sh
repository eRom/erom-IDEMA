#!/bin/bash
# Construit IdemA.app dans le dossier cible de Cargo : binaire release, Info.plist, icône, signature ad hoc
# (lancement local). Demande rsvg-convert (`brew install librsvg`) et iconutil (macOS).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
TARGET="${CARGO_TARGET_DIR:-$ROOT/target}"
APP="$TARGET/IdemA.app"
# Les outils d'icône manquants se signalent avant la compilation, qui est longue.
for tool in rsvg-convert iconutil; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "bundle-app.sh: missing required tool '$tool' (rsvg-convert: brew install librsvg; iconutil ships with macOS)" >&2
    exit 1
  fi
done
VERSION="$(scripts/version.sh)"
cargo build --release --locked -p idema
# Le binaire doit dire la version qu'on va écrire dans le bundle (menu Aide, `idema --version`).
built="$("$TARGET/release/idema" --version)"
if [ "$built" != "IdemA $VERSION" ]; then
  echo "bundle-app.sh: the binary says '$built', Cargo.toml says '$VERSION'" >&2
  exit 1
fi

WORK="$(mktemp -d)"
# Le dossier de travail disparaît à la sortie, succès ou échec.
trap 'rm -rf "$WORK"' EXIT
CONTENTS="$WORK/IdemA.app/Contents"
mkdir -p "$CONTENTS/MacOS" "$CONTENTS/Resources" "$WORK/IdemA.iconset"
cp "$TARGET/release/idema" "$CONTENTS/MacOS/idema"
cp packaging/Info.plist "$CONTENTS/Info.plist"
# La version n'est écrite qu'ici : le plist du dépôt n'en porte pas, il ne peut pas diverger de Cargo.toml.
plutil -replace CFBundleShortVersionString -string "$VERSION" "$CONTENTS/Info.plist"
plutil -replace CFBundleVersion -string "$VERSION" "$CONTENTS/Info.plist"
plutil -lint "$CONTENTS/Info.plist"
for size in 16 32 128 256 512; do
  rsvg-convert -w "$size" -h "$size" packaging/icon.svg -o "$WORK/IdemA.iconset/icon_${size}x${size}.png"
  rsvg-convert -w "$((size * 2))" -h "$((size * 2))" packaging/icon.svg -o "$WORK/IdemA.iconset/icon_${size}x${size}@2x.png"
done
iconutil -c icns "$WORK/IdemA.iconset" -o "$CONTENTS/Resources/Idema.icns"
codesign --force --sign - "$WORK/IdemA.app"
# APFS ignore la casse : rsync dans une `Idema.app` existante en garde le nom. Tout dossier au nom de l'app dans une
# autre casse est renommé en deux temps (un nom intermédiaire, puis `IdemA.app`) avant la copie.
for entry in "$TARGET"/*; do
  name="$(basename "$entry")"
  if [ "$(printf '%s' "$name" | tr '[:upper:]' '[:lower:]')" = "idema.app" ] && [ "$name" != "IdemA.app" ]; then
    mv "$entry" "$TARGET/.IdemA.app.renaming"
    mv "$TARGET/.IdemA.app.renaming" "$APP"
  fi
done
mkdir -p "$APP"
# Remplace le contenu de l'app précédente, sans toucher à rien d'autre.
rsync -a --delete "$WORK/IdemA.app/" "$APP/"
echo "$APP"
