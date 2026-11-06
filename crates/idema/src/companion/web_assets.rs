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

const HTML: &str = "text/html; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";
const JS: &str = "text/javascript; charset=utf-8";
const PNG: &str = "image/png";

/// Les fichiers de la page : chemin, octets, type. Un module JS oublié ici est un 404 au chargement : l'app reste
/// blanche, d'où le test qui compare cette liste au dossier.
const FILES: &[(&str, &[u8], &str)] = &[
    ("/", include_bytes!("../../companion-web/index.html"), HTML),
    (
        "/app.css",
        include_bytes!("../../companion-web/app.css"),
        CSS,
    ),
    (
        "/manifest.webmanifest",
        include_bytes!("../../companion-web/manifest.webmanifest"),
        "application/manifest+json",
    ),
    ("/sw.js", include_bytes!("../../companion-web/sw.js"), JS),
    (
        "/js/api.js",
        include_bytes!("../../companion-web/js/api.js"),
        JS,
    ),
    (
        "/js/dom.js",
        include_bytes!("../../companion-web/js/dom.js"),
        JS,
    ),
    (
        "/js/icons.js",
        include_bytes!("../../companion-web/js/icons.js"),
        JS,
    ),
    (
        "/js/install.js",
        include_bytes!("../../companion-web/js/install.js"),
        JS,
    ),
    (
        "/js/main.js",
        include_bytes!("../../companion-web/js/main.js"),
        JS,
    ),
    (
        "/js/notify.js",
        include_bytes!("../../companion-web/js/notify.js"),
        JS,
    ),
    (
        "/js/offline.js",
        include_bytes!("../../companion-web/js/offline.js"),
        JS,
    ),
    (
        "/js/pairing.js",
        include_bytes!("../../companion-web/js/pairing.js"),
        JS,
    ),
    (
        "/js/presence.js",
        include_bytes!("../../companion-web/js/presence.js"),
        JS,
    ),
    (
        "/js/push.js",
        include_bytes!("../../companion-web/js/push.js"),
        JS,
    ),
    (
        "/js/router.js",
        include_bytes!("../../companion-web/js/router.js"),
        JS,
    ),
    (
        "/js/shell.js",
        include_bytes!("../../companion-web/js/shell.js"),
        JS,
    ),
    (
        "/js/store.js",
        include_bytes!("../../companion-web/js/store.js"),
        JS,
    ),
    (
        "/js/time.js",
        include_bytes!("../../companion-web/js/time.js"),
        JS,
    ),
    (
        "/icons/icon-180.png",
        include_bytes!("../../companion-web/icons/icon-180.png"),
        PNG,
    ),
    (
        "/icons/icon-512.png",
        include_bytes!("../../companion-web/icons/icon-512.png"),
        PNG,
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

    /// Le type attendu d'un chemin, d'après son extension : la liste ne se juge pas sur elle-même.
    fn expected_type(path: &str) -> &'static str {
        match path.rsplit('.').next() {
            Some("html") => "text/html; charset=utf-8",
            Some("css") => "text/css; charset=utf-8",
            Some("js") => "text/javascript; charset=utf-8",
            Some("webmanifest") => "application/manifest+json",
            Some("png") => "image/png",
            other => panic!("extension without a known type: {other:?}"),
        }
    }

    #[test]
    fn every_listed_file_is_served_with_its_type_and_an_etag() {
        for (path, _, _) in FILES {
            let served = asset(path).unwrap_or_else(|| panic!("{path} is not served"));
            assert!(!served.bytes.is_empty(), "{path} is empty");
            let expected = if *path == "/" {
                expected_type("/index.html")
            } else {
                expected_type(path)
            };
            assert_eq!(served.content_type, expected, "{path}");
            assert!(
                served.etag.len() > 2 && served.etag.starts_with('"') && served.etag.ends_with('"'),
                "{path}: {}",
                served.etag
            );
        }
    }

    #[test]
    fn the_list_covers_every_file_of_the_app_folder() {
        // Un fichier ajouté au dossier sans l'être à la liste serait un 404 : un module manquant laisse la page blanche.
        let folder = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("companion-web");
        let mut on_disk = vec!["/app.css".to_owned(), "/sw.js".to_owned()];
        for (subfolder, extension) in [("js", "js"), ("icons", "png")] {
            for entry in std::fs::read_dir(folder.join(subfolder)).expect("app subfolder") {
                let name = entry
                    .expect("entry")
                    .file_name()
                    .into_string()
                    .expect("utf-8 name");
                if name.ends_with(extension) {
                    on_disk.push(format!("/{subfolder}/{name}"));
                }
            }
        }
        for path in on_disk {
            assert!(
                asset(&path).is_some(),
                "{path} exists in companion-web but is not served"
            );
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
