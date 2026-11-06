//! Le QR code de l'adresse tailnet : la matrice de modules que la section « Compagnon » dessine, un carré par module.

use qrcodegen::{QrCode, QrCodeEcc};

/// Les modules du QR code de `text` : une matrice carrée (`true` : module sombre), sans la zone de silence que le
/// dessin ajoute lui-même. Correction d'erreur moyenne : une adresse tient dans la version 5 ou moins. Vide si le texte
/// ne tient dans aucune version (il ne peut pas arriver pour une adresse, bornée par la config).
pub fn qr_modules(text: &str) -> Vec<Vec<bool>> {
    let Ok(code) = QrCode::encode_text(text, QrCodeEcc::Medium) else {
        return Vec::new();
    };
    let size = code.size();
    (0..size)
        .map(|y| (0..size).map(|x| code.get_module(x, y)).collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qr_round_trip() {
        let modules = qr_modules("https://mac.example.ts.net:8455");
        let side = modules.len();
        assert!(side >= 21, "a QR code is at least 21 modules wide");
        assert_eq!(side % 2, 1, "the side of a QR code is odd");
        assert!(modules.iter().all(|row| row.len() == side), "square");
        // Les trois repères de coin : un cadre sombre de 7 modules, avec un cœur de 3.
        for (x0, y0) in [(0, 0), (side - 7, 0), (0, side - 7)] {
            for d in 0..7 {
                assert!(modules[y0][x0 + d] && modules[y0 + 6][x0 + d], "finder row");
                assert!(
                    modules[y0 + d][x0] && modules[y0 + d][x0 + 6],
                    "finder column"
                );
            }
            assert!(modules[y0 + 3][x0 + 3], "finder core");
            assert!(!modules[y0 + 1][x0 + 1], "finder gap");
        }
    }

    #[test]
    fn different_texts_give_different_codes() {
        assert_ne!(
            qr_modules("https://a.example"),
            qr_modules("https://b.example")
        );
    }
}
