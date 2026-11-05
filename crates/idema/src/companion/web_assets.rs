//! Les fichiers de la page du téléphone, embarqués dans le binaire (`crates/idema/companion-web/`) et servis sans
//! cookie ni donnée. Chacun porte un `ETag` (empreinte de son contenu) pour que le navigateur revalide sans retélécharger.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use sha2::{Digest, Sha256};

use crate::assets::logo_bytes;

/// Un fichier à servir.
#[derive(Debug, Clone, Copy)]
pub struct Asset {
    pub bytes: &'static [u8],
    pub content_type: &'static str,
    /// Entre guillemets, comme l'exige l'en-tête.
    pub etag: &'static str,
}

/// Les fichiers de la page : chemin, octets, type.
const FILES: &[(&str, &[u8], &str)] = &[
    (
        "/",
        include_bytes!("../../companion-web/index.html"),
        "text/html; charset=utf-8",
    ),
    (
        "/manifest.webmanifest",
        include_bytes!("../../companion-web/manifest.webmanifest"),
        "application/manifest+json",
    ),
    (
        "/sw.js",
        include_bytes!("../../companion-web/sw.js"),
        "text/javascript; charset=utf-8",
    ),
    (
        "/icons/icon-180.png",
        include_bytes!("../../companion-web/icons/icon-180.png"),
        "image/png",
    ),
    (
        "/icons/icon-512.png",
        include_bytes!("../../companion-web/icons/icon-512.png"),
        "image/png",
    ),
];

/// Le fichier servi à `path` (chemin d'URL, sans paramètres), s'il y en a un. Aucune lecture du disque : seuls les
/// fichiers listés ici et les logos de `assets.rs` existent, donc aucun chemin ne peut sortir de la liste.
pub fn asset(path: &str) -> Option<Asset> {
    let path = if path == "/index.html" { "/" } else { path };
    if let Some((_, bytes, content_type)) = FILES.iter().find(|(known, ..)| *known == path) {
        return Some(Asset {
            bytes,
            content_type,
            etag: etag_of(path, bytes),
        });
    }
    let name = path.strip_prefix("/logos/")?.strip_suffix(".png")?;
    let bytes = logo_bytes(name)?;
    Some(Asset {
        bytes,
        content_type: "image/png",
        etag: etag_of(path, bytes),
    })
}

/// L'`ETag` d'un fichier, calculé une fois par chemin (la chaîne vit autant que le process : le nombre de chemins
/// possibles est borné par la liste ci-dessus et par les logos embarqués).
fn etag_of(path: &str, bytes: &[u8]) -> &'static str {
    static CACHE: OnceLock<Mutex<HashMap<String, &'static str>>> = OnceLock::new();
    let mut cache = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(etag) = cache.get(path) {
        return etag;
    }
    let digest = Sha256::digest(bytes);
    let short: String = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let etag: &'static str = Box::leak(format!("\"{short}\"").into_boxed_str());
    cache.insert(path.to_owned(), etag);
    etag
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_app_files_are_served_with_their_type() {
        let index = asset("/").expect("the shell page");
        assert_eq!(index.content_type, "text/html; charset=utf-8");
        assert!(index.bytes.starts_with(b"<!doctype html>"));
        assert_eq!(asset("/index.html").unwrap().bytes, index.bytes);
        assert_eq!(
            asset("/manifest.webmanifest").unwrap().content_type,
            "application/manifest+json"
        );
        assert!(
            asset("/sw.js")
                .unwrap()
                .content_type
                .starts_with("text/javascript")
        );
        for icon in ["/icons/icon-180.png", "/icons/icon-512.png"] {
            let png = asset(icon).unwrap();
            assert!(png.bytes.starts_with(b"\x89PNG\r\n\x1a\n"), "{icon}");
        }
    }

    #[test]
    fn logos_come_from_the_embedded_set_only() {
        let claude = asset("/logos/claude.png").expect("an embedded logo");
        assert_eq!(claude.content_type, "image/png");
        assert_eq!(Some(claude.bytes), logo_bytes("claude"));
        for hostile in [
            "/logos/absent.png",
            "/logos/../index.html",
            "/logos/..%2Findex.html",
            "/logos/claude",
            "/logos/",
            "/logos/claude.png/",
            "/idema-api/health",
            "/etc/passwd",
        ] {
            assert!(asset(hostile).is_none(), "{hostile}");
        }
    }

    #[test]
    fn the_etag_follows_the_content() {
        let first = asset("/sw.js").unwrap();
        assert_eq!(first.etag, asset("/sw.js").unwrap().etag);
        assert!(first.etag.starts_with('"') && first.etag.ends_with('"'));
        assert_ne!(first.etag, asset("/").unwrap().etag);
        assert_ne!(
            asset("/logos/claude.png").unwrap().etag,
            asset("/logos/codex.png").unwrap().etag
        );
    }
}
