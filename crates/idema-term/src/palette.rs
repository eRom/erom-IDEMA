//! La palette ANSI d'Idema (maquette « ANSI ») : de la couleur d'une cellule à un triplet RVB.

use alacritty_terminal::vte::ansi::{Color, NamedColor};

/// Fond et texte par défaut du terminal.
pub const DEFAULT_BACKGROUND: [u8; 3] = [0x15, 0x15, 0x16];
pub const DEFAULT_FOREGROUND: [u8; 3] = [0xE2, 0xE2, 0xE3];

/// Les seize couleurs ANSI : huit normales, puis huit vives.
const ANSI: [[u8; 3]; 16] = [
    [0x1E, 0x1E, 0x20],
    [0xFF, 0x69, 0x61],
    [0xA8, 0xD6, 0x72],
    [0xFF, 0xD6, 0x0A],
    [0x3A, 0xA0, 0xFF],
    [0xC5, 0x8A, 0xF9],
    [0x64, 0xC3, 0xF5],
    [0xD4, 0xD4, 0xD6],
    [0x5A, 0x5A, 0x5E],
    [0xFF, 0x8A, 0x80],
    [0xC2, 0xE8, 0x9A],
    [0xFF, 0xE5, 0x66],
    [0x7D, 0xBB, 0xFF],
    [0xDD, 0xB3, 0xFF],
    [0x9A, 0xDC, 0xF9],
    [0xFF, 0xFF, 0xFF],
];

/// Niveaux d'un axe du cube 6x6x6 (index 16 à 231).
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// Triplet RVB d'une couleur de cellule. `fg` dit si la couleur sert au texte ou au fond :
/// il départage le curseur, qui n'a pas de couleur propre dans la palette (texte : clair, fond : sombre).
pub fn rgb(color: Color, fg: bool) -> [u8; 3] {
    match color {
        Color::Spec(spec) => [spec.r, spec.g, spec.b],
        Color::Indexed(index) => indexed(index),
        Color::Named(named) => named_rgb(named, fg),
    }
}

fn indexed(index: u8) -> [u8; 3] {
    match index {
        0..=15 => ANSI[usize::from(index)],
        16..=231 => {
            let cube = usize::from(index - 16);
            [
                CUBE_LEVELS[cube / 36],
                CUBE_LEVELS[cube / 6 % 6],
                CUBE_LEVELS[cube % 6],
            ]
        }
        232..=255 => {
            let grey = 8 + 10 * (index - 232);
            [grey, grey, grey]
        }
    }
}

/// Estompe une couleur de texte déjà résolue (SGR 2) : deux tiers de l'intensité.
pub fn dim(color: [u8; 3]) -> [u8; 3] {
    color.map(|channel| (u16::from(channel) * 2 / 3) as u8)
}

fn named_rgb(named: NamedColor, fg: bool) -> [u8; 3] {
    use NamedColor::*;
    match named {
        Black | Red | Green | Yellow | Blue | Magenta | Cyan | White | BrightBlack | BrightRed
        | BrightGreen | BrightYellow | BrightBlue | BrightMagenta | BrightCyan | BrightWhite => {
            ANSI[named as usize]
        }
        Foreground => DEFAULT_FOREGROUND,
        Background => DEFAULT_BACKGROUND,
        Cursor if fg => DEFAULT_FOREGROUND,
        Cursor => DEFAULT_BACKGROUND,
        BrightForeground => ANSI[15],
        DimForeground => dim(DEFAULT_FOREGROUND),
        DimBlack => dim(ANSI[0]),
        DimRed => dim(ANSI[1]),
        DimGreen => dim(ANSI[2]),
        DimYellow => dim(ANSI[3]),
        DimBlue => dim(ANSI[4]),
        DimMagenta => dim(ANSI[5]),
        DimCyan => dim(ANSI[6]),
        DimWhite => dim(ANSI[7]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::vte::ansi::Rgb;

    fn hex(value: u32) -> [u8; 3] {
        [(value >> 16) as u8, (value >> 8) as u8, value as u8]
    }

    #[test]
    fn named_red_is_the_mockup_red() {
        assert_eq!(rgb(Color::Named(NamedColor::Red), true), hex(0xFF6961));
    }

    #[test]
    fn named_bright_red_is_the_mockup_bright_red() {
        assert_eq!(
            rgb(Color::Named(NamedColor::BrightRed), true),
            hex(0xFF8A80)
        );
    }

    #[test]
    fn indexed_four_is_the_mockup_blue() {
        assert_eq!(rgb(Color::Indexed(4), true), hex(0x3AA0FF));
    }

    #[test]
    fn indexed_196_is_pure_red_from_the_color_cube() {
        assert_eq!(rgb(Color::Indexed(196), true), [255, 0, 0]);
    }

    #[test]
    fn indexed_232_is_the_first_grey_of_the_ramp() {
        assert_eq!(rgb(Color::Indexed(232), true), [8, 8, 8]);
    }

    #[test]
    fn truecolor_spec_is_passed_through() {
        assert_eq!(rgb(Color::Spec(Rgb { r: 1, g: 2, b: 3 }), true), [1, 2, 3]);
    }

    #[test]
    fn default_foreground_is_the_mockup_foreground() {
        assert_eq!(
            rgb(Color::Named(NamedColor::Foreground), true),
            hex(0xE2E2E3)
        );
    }

    #[test]
    fn default_background_is_the_mockup_background() {
        assert_eq!(
            rgb(Color::Named(NamedColor::Background), false),
            hex(0x151516)
        );
    }
}
