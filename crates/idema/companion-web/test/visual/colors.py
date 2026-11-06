"""Les couleurs des jetons dans les captures (tâche 16, AC-20).

Pour chaque écran, lit la capture 3x (`<préfixe><écran>.png`) et la liste de ses points nommés (`<écran>.points.json`,
en pixels CSS, écrite par capture.sh), et vérifie que le pixel de chaque point a la couleur du jeton du §3.11 de la spec,
à 3 unités près par canal. Sort en non-zéro à la première couleur fausse, à un point absent ou à une capture qui n'a pas
la taille de la fenêtre de l'iPhone 15 Pro Max à 3x.

Usage : python3 -I colors.py <dossier des captures> [préfixe, « t16- » par défaut]
"""

import json
import sys
from pathlib import Path

from PIL import Image

SCALE = 3
WIDTH, HEIGHT = 430 * SCALE, 932 * SCALE
TOLERANCE = 3

# Les jetons du §3.11 (fond, cartes, barres, pastilles, accent) et le creux de l'écran du pane (`--screen-bg`).
TOKENS = {
    "bg": (0x15, 0x15, 0x16),
    "card": (0x1C, 0x1C, 0x1E),
    "bar": (0x1A, 0x1A, 0x1C),
    "banner": (0x1A, 0x1A, 0x1C),
    "screen": (0x10, 0x10, 0x11),
    "dot-waiting": (0xFF, 0xD6, 0x0A),
    "dot-running": (0x30, 0xD1, 0x58),
    "dot-error": (0xFF, 0x45, 0x3A),
    "accent": (0x0A, 0x84, 0xFF),
}

# Les points exigés par écran : un écran dont la liste n'a pas l'un d'eux ne passe pas (rien n'est vérifié par omission).
REQUIRED = {
    "install": ["bg", "card"],
    "pairing": ["bg"],
    "fleet": ["bg", "card", "bar", "dot-waiting", "dot-running", "dot-error"],
    # Pas de fond (#151516) sur la session : l'en-tête, le creux de l'écran et la barre de réponse le recouvrent.
    "session": ["bar", "card", "screen", "accent"],
    "absence": ["bg", "card", "bar"],
    "notes": ["bg", "card", "bar"],
    "offline": ["bg", "card", "banner"],
}


def hexcode(rgb):
    return "#%02X%02X%02X" % rgb[:3]


def check_screen(folder: Path, prefix: str, screen: str) -> list[str]:
    """Les écarts de cet écran, vides si tout est dans la tolérance."""
    failures = []
    image_path = folder / f"{prefix}{screen}.png"
    points_path = folder / f"{prefix}{screen}.points.json"
    if not image_path.exists() or not points_path.exists():
        return [f"{screen}: capture or points file missing ({image_path.name}, {points_path.name})"]
    image = Image.open(image_path).convert("RGB")
    if image.size != (WIDTH, HEIGHT):
        failures.append(f"{screen}: capture is {image.size[0]}x{image.size[1]}, expected {WIDTH}x{HEIGHT}")
    points = json.loads(points_path.read_text())
    for name in REQUIRED[screen]:
        if name not in points:
            failures.append(f"{screen}: point '{name}' not recorded")
            continue
        x, y = points[name]
        px, py = int(x * SCALE), int(y * SCALE)
        if not (0 <= px < image.size[0] and 0 <= py < image.size[1]):
            failures.append(f"{screen}: point '{name}' at ({x:.1f}, {y:.1f}) is outside the capture")
            continue
        actual = image.getpixel((px, py))
        expected = TOKENS[name]
        off = max(abs(a - e) for a, e in zip(actual, expected))
        verdict = "ok  " if off <= TOLERANCE else "FAIL"
        print(
            f"  {verdict} {screen:8} {name:12} ({x:7.1f},{y:7.1f}) px=({px:4},{py:4}) "
            f"found {hexcode(actual)} expected {hexcode(expected)} max channel gap {off}"
        )
        if off > TOLERANCE:
            failures.append(f"{screen}: '{name}' is {hexcode(actual)}, expected {hexcode(expected)} +-{TOLERANCE}")
    return failures


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    folder = Path(sys.argv[1])
    prefix = sys.argv[2] if len(sys.argv) > 2 else "t16-"
    failures = []
    for screen in REQUIRED:
        failures += check_screen(folder, prefix, screen)
    if failures:
        print("COLORS FAIL")
        for failure in failures:
            print(f"  - {failure}")
        return 1
    checked = sum(len(names) for names in REQUIRED.values())
    print(f"COLORS PASS ({checked} points on {len(REQUIRED)} screens, tolerance {TOLERANCE})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
