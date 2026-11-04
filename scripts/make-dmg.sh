#!/bin/bash
# Emballe IdemA.app (construite par scripts/bundle-app.sh) dans un DMG à glisser vers Applications :
# <cible>/IdemA-<version>-arm64.dmg, plus son empreinte .sha256. hdiutil seul, sans Finder ni AppleScript : tourne sans
# session graphique (runner de CI).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
TARGET="${CARGO_TARGET_DIR:-$ROOT/target}"
APP="$TARGET/IdemA.app"
VERSION="$(scripts/version.sh)"
if [ ! -d "$APP" ]; then
  echo "make-dmg.sh: $APP is missing, run scripts/bundle-app.sh first" >&2
  exit 1
fi
# Le bundle doit être celui de cette version, pour Apple Silicon, et sa signature (ad hoc) intacte.
bundled="$(plutil -extract CFBundleShortVersionString raw "$APP/Contents/Info.plist")"
if [ "$bundled" != "$VERSION" ]; then
  echo "make-dmg.sh: the bundle says $bundled, Cargo.toml says $VERSION" >&2
  exit 1
fi
archs="$(lipo -archs "$APP/Contents/MacOS/idema")"
if [ "$archs" != "arm64" ]; then
  echo "make-dmg.sh: binary architectures '$archs', expected arm64" >&2
  exit 1
fi
codesign --verify --strict "$APP"

DMG="$TARGET/IdemA-$VERSION-arm64.dmg"
STAGE="$(mktemp -d)"
# Le dossier d'assemblage disparaît à la sortie, succès ou échec.
trap 'rm -rf "$STAGE"' EXIT
ditto "$APP" "$STAGE/IdemA.app"
ln -s /Applications "$STAGE/Applications"
# Sur les runners, hdiutil échoue parfois en « Resource busy » (XProtect tient l'image neuve) : trois essais.
for attempt in 1 2 3; do
  if hdiutil create -volname "IdemA $VERSION" -srcfolder "$STAGE" -ov -format UDZO "$DMG"; then
    break
  fi
  if [ "$attempt" = 3 ]; then
    echo "make-dmg.sh: hdiutil create failed three times" >&2
    exit 1
  fi
  sleep 5
done
hdiutil verify "$DMG"
(cd "$TARGET" && shasum -a 256 "$(basename "$DMG")" >"$(basename "$DMG").sha256")
echo "$DMG"
