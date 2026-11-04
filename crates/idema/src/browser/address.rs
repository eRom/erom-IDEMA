//! Ce que l'utilisateur tape dans la barre d'adresse, lu comme une adresse web, un fichier ou une recherche.
//! Aucune fenêtre ici : de l'analyse de texte, testable seule.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use url::Url;

/// Le moteur interrogé quand le texte n'est ni une adresse ni un chemin.
pub const SEARCH_URL: &str = "https://www.google.com/search?q=";

#[derive(Debug, Clone, PartialEq)]
pub enum Address {
    Web(Url),
    File(PathBuf),
    Search(String),
}

/// Lit le texte de la barre d'adresse. `None` seulement quand il est vide.
pub fn interpret(text: &str, home: &Path) -> Option<Address> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if text.starts_with('/') {
        return Some(Address::File(PathBuf::from(text)));
    }
    if text == "~" {
        return Some(Address::File(home.to_path_buf()));
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return Some(Address::File(home.join(rest)));
    }
    let search = || Some(Address::Search(text.to_string()));
    let lower = text.to_ascii_lowercase();
    if ["http://", "https://", "file://"]
        .iter()
        .any(|scheme| lower.starts_with(scheme))
    {
        let Ok(url) = Url::parse(text) else {
            return search();
        };
        if url.scheme() == "file" {
            return match url.to_file_path() {
                Ok(path) => Some(Address::File(path)),
                Err(()) => search(),
            };
        }
        return Some(Address::Web(url));
    }
    if text.contains(char::is_whitespace) {
        return search();
    }
    let guessed = if is_local_host(&lower) {
        format!("http://{text}")
    } else if text.contains('.') {
        format!("https://{text}")
    } else {
        return search();
    };
    match Url::parse(&guessed) {
        Ok(url) => Some(Address::Web(url)),
        Err(_) => search(),
    }
}

/// Là où vivent les serveurs de développement, donc en `http://` : `localhost` et `*.localhost`, `[::1]`, `0.0.0.0`,
/// la boucle locale `127.x` et les réseaux privés (`10.x`, `172.16-31.x`, `192.168.x`), suivis de rien, de `:`,
/// de `/`, de `?` ou de `#`.
fn is_local_host(lower: &str) -> bool {
    let at_boundary = |rest: &str| rest.is_empty() || rest.starts_with([':', '/', '?', '#']);
    if let Some(rest) = lower.strip_prefix("[::1]") {
        return at_boundary(rest);
    }
    let host = lower.split([':', '/', '?', '#']).next().unwrap_or("");
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    host.parse::<Ipv4Addr>()
        .is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified() || ip.is_private())
}

/// L'adresse que le moteur web charge réellement.
pub fn url_of(address: &Address) -> String {
    match address {
        Address::Web(url) => url.to_string(),
        Address::File(path) => match Url::from_file_path(path) {
            Ok(url) => url.to_string(),
            // Chemin relatif : pas d'adresse canonique, on garde la forme brute.
            Err(()) => format!("file://{}", path.display()),
        },
        Address::Search(text) => {
            let encoded: String = url::form_urlencoded::byte_serialize(text.as_bytes()).collect();
            format!("{SEARCH_URL}{encoded}")
        }
    }
}

/// Deux adresses qui ne diffèrent que par le fragment désignent la même page. Une adresse illisible
/// n'est égale qu'à elle-même.
pub fn same_address(a: &str, b: &str) -> bool {
    match (Url::parse(a), Url::parse(b)) {
        (Ok(mut a), Ok(mut b)) => {
            a.set_fragment(None);
            b.set_fragment(None);
            a == b
        }
        _ => a == b,
    }
}

/// La forme qu'on écrit dans un journal : ni requête, ni fragment, ni identifiants (ils portent des secrets,
/// un jeton de rappel OAuth par exemple). Un fichier devient son chemin ; ce qui ne se lit pas, `-`.
pub fn log_form(url: &str) -> String {
    let Ok(mut url) = Url::parse(url) else {
        return "-".to_string();
    };
    match url.scheme() {
        "file" => match url.to_file_path() {
            Ok(path) => path.display().to_string(),
            Err(()) => "-".to_string(),
        },
        "http" | "https" => {
            url.set_query(None);
            url.set_fragment(None);
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.to_string()
        }
        _ => "-".to_string(),
    }
}

/// La forme d'un lien du terminal dans le journal : celle de `log_form` pour le web et les fichiers ; une adresse
/// `mailto:` garde son destinataire sans requête ni fragment (le système l'ouvre, le journal le dit) ; tout autre
/// schéma se réduit à son nom (`javascript:`), jamais à son contenu.
pub fn log_link(uri: &str) -> String {
    let Ok(url) = Url::parse(uri) else {
        return "-".to_string();
    };
    match url.scheme() {
        "http" | "https" | "file" => log_form(uri),
        "mailto" => format!("mailto:{}", url.path()),
        scheme => format!("{scheme}:"),
    }
}

/// L'adresse qu'un onglet garde pour `url`. Un fichier local se charge par `WebPage::load_file`, qui ne sait honorer ni
/// requête ni fragment : l'adresse montrée doit être fidèle à ce qui est affiché, elle les perd donc à l'entrée dans
/// l'onglet. Une adresse web les garde, mais `[::]` et `0.0.0.0` (ce qu'un serveur écrit pour dire « toutes les interfaces »,
/// `http://[::]:8811/` pour `python3 -m http.server`) deviennent `localhost`, le même port et le même chemin : ce ne sont
/// pas des adresses où l'on se connecte, le moteur web les refuse (« bad URL »).
pub fn tab_address(url: &str) -> String {
    match Url::parse(url) {
        Ok(mut parsed) if parsed.scheme() == "file" => {
            parsed.set_query(None);
            parsed.set_fragment(None);
            parsed.to_string()
        }
        Ok(mut parsed)
            if matches!(parsed.scheme(), "http" | "https") && is_unspecified_host(&parsed) =>
        {
            match parsed.set_host(Some("localhost")) {
                Ok(()) => parsed.to_string(),
                Err(_) => url.to_owned(),
            }
        }
        _ => url.to_owned(),
    }
}

/// L'hôte de `url` est l'adresse « non spécifiée » : `[::]` ou `0.0.0.0`.
fn is_unspecified_host(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_unspecified(),
        Some(url::Host::Ipv6(ip)) => ip.is_unspecified(),
        _ => false,
    }
}

/// Un document local que le navigateur sait afficher : HTML ou PDF, quelle que soit la casse.
pub fn is_local_document(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            ["html", "htm", "pdf"]
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known))
        })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn web(text: &str) -> String {
        url_of(&interpret(text, Path::new("/Users/r")).unwrap())
    }

    #[test]
    fn interprets_local_hosts_paths_and_searches() {
        let home = Path::new("/Users/r");
        assert_eq!(web("localhost:3000"), "http://localhost:3000/");
        assert_eq!(web("localhost"), "http://localhost/");
        assert_eq!(web("127.0.0.1:8080/a?b=1"), "http://127.0.0.1:8080/a?b=1");
        assert_eq!(web("[::1]:3000"), "http://[::1]:3000/");
        assert_eq!(web("vercel.com"), "https://vercel.com/");
        assert_eq!(web("HTTP://Example.com/X"), "http://example.com/X");
        assert_eq!(
            interpret("~/dev/a b/é.html", home),
            Some(Address::File("/Users/r/dev/a b/é.html".into()))
        );
        assert_eq!(
            interpret("/tmp/r.pdf", home),
            Some(Address::File("/tmp/r.pdf".into()))
        );
        assert_eq!(
            interpret("file:///tmp/a%20b.html", home),
            Some(Address::File("/tmp/a b.html".into()))
        );
        assert_eq!(
            interpret("next dev ports", home),
            Some(Address::Search("next dev ports".into()))
        );
        assert_eq!(
            interpret("a.b c", home),
            Some(Address::Search("a.b c".into()))
        );
        assert_eq!(interpret("   ", home), None);
        assert_eq!(
            url_of(&Address::Search("é & co".into())),
            "https://www.google.com/search?q=%C3%A9+%26+co"
        );
    }

    #[test]
    fn same_address_ignores_only_the_fragment() {
        assert!(same_address(
            "http://localhost:8811/",
            "http://localhost:8811/#top"
        ));
        assert!(same_address(
            "http://localhost:8811",
            "http://localhost:8811/"
        ));
        assert!(!same_address(
            "http://localhost:8811/?a=1",
            "http://localhost:8811/?a=2"
        ));
        assert!(!same_address(
            "http://localhost:8811/",
            "https://localhost:8811/"
        ));
        assert!(!same_address(
            "http://localhost:8811/a",
            "http://localhost:8811/b"
        ));
    }

    #[test]
    fn the_log_form_keeps_neither_query_nor_fragment() {
        assert_eq!(
            log_form("http://localhost:8811/cb?code=abc#token=def"),
            "http://localhost:8811/cb"
        );
        assert_eq!(log_form("file:///tmp/a%20b.html?x"), "/tmp/a b.html");
        assert_eq!(log_form("not a url"), "-");
    }

    #[test]
    fn a_file_address_loses_its_query_and_fragment_when_it_enters_a_tab() {
        assert_eq!(
            tab_address("file:///tmp/a%20b.pdf#page=2"),
            "file:///tmp/a%20b.pdf"
        );
        assert_eq!(
            tab_address("file:///tmp/site/index.html?x=1#top"),
            "file:///tmp/site/index.html"
        );
        assert_eq!(tab_address("FILE:///tmp/a.html#x"), "file:///tmp/a.html");
        // Une adresse web garde sa requête et son fragment, une adresse illisible reste telle quelle.
        assert_eq!(
            tab_address("http://localhost:8811/a?b=1#c"),
            "http://localhost:8811/a?b=1#c"
        );
        assert_eq!(tab_address("pas une adresse"), "pas une adresse");
    }

    #[test]
    fn an_all_interfaces_address_opens_on_localhost_with_its_port_and_path() {
        assert_eq!(tab_address("http://[::]:8811/"), "http://localhost:8811/");
        assert_eq!(
            tab_address("http://0.0.0.0:3000/app?x=1#top"),
            "http://localhost:3000/app?x=1#top"
        );
        assert_eq!(tab_address("https://0.0.0.0/a"), "https://localhost/a");
        // Un hôte précis, y compris la boucle locale, reste tel quel.
        assert_eq!(tab_address("http://[::1]:8811/"), "http://[::1]:8811/");
        assert_eq!(
            tab_address("http://127.0.0.1:8811/"),
            "http://127.0.0.1:8811/"
        );
        assert_eq!(
            tab_address("http://192.168.1.4:80/"),
            "http://192.168.1.4:80/"
        );
    }

    #[test]
    fn the_log_link_form_keeps_no_query_fragment_or_payload() {
        assert_eq!(
            log_link("https://u:p@h.dev/cb?code=abc#token=def"),
            "https://h.dev/cb"
        );
        assert_eq!(log_link("file:///tmp/a.html?x#y"), "/tmp/a.html");
        assert_eq!(
            log_link("mailto:x@example.com?subject=secret&body=s#f"),
            "mailto:x@example.com"
        );
        assert_eq!(log_link("javascript:alert('secret')"), "javascript:");
        assert_eq!(log_link("data:text/html,secret"), "data:");
        assert_eq!(log_link("not a url"), "-");
    }

    #[test]
    fn the_log_form_drops_credentials_and_unknown_schemes() {
        assert_eq!(log_form("http://u:p@h/x?a#b"), "http://h/x");
        assert_eq!(log_form("https://token@h.dev/x"), "https://h.dev/x");
        assert_eq!(log_form("javascript:alert(1)"), "-");
        assert_eq!(log_form("data:text/html,secret"), "-");
        assert_eq!(log_form("about:blank"), "-");
        assert_eq!(log_form("ftp://h/x"), "-");
    }

    #[test]
    fn a_non_web_scheme_is_never_loaded_as_an_address() {
        let home = Path::new("/Users/r");
        assert_eq!(
            interpret("javascript:alert(1)", home),
            Some(Address::Search("javascript:alert(1)".into()))
        );
        assert_eq!(
            interpret("data:text/html,x", home),
            Some(Address::Search("data:text/html,x".into()))
        );
        // Même avec un point : la lecture « https:// + texte » ne doit pas fabriquer d'adresse chargeable.
        for text in [
            "javascript:alert(document.cookie)",
            "data:text/html,<b>a.b</b>",
        ] {
            let address = interpret(text, home).unwrap();
            assert!(
                matches!(address, Address::Search(_)),
                "{text} -> {address:?}"
            );
        }
    }

    #[test]
    fn a_query_or_fragment_right_after_localhost_keeps_it_local() {
        assert_eq!(web("localhost?x=1"), "http://localhost/?x=1");
        assert_eq!(web("localhost#x"), "http://localhost/#x");
        assert_eq!(web("localhost/a"), "http://localhost/a");
        // Un nom qui ne fait que commencer par localhost n'est pas local.
        assert_eq!(web("localhostage.dev"), "https://localhostage.dev/");
    }

    #[test]
    fn dev_server_hosts_get_http() {
        assert_eq!(web("0.0.0.0:3000"), "http://0.0.0.0:3000/");
        assert_eq!(web("app.localhost:3000"), "http://app.localhost:3000/");
        assert_eq!(web("APP.Localhost"), "http://app.localhost/");
        assert_eq!(web("10.0.0.5:8080"), "http://10.0.0.5:8080/");
        assert_eq!(web("192.168.1.20:5173/x"), "http://192.168.1.20:5173/x");
        assert_eq!(web("172.16.0.1:80"), "http://172.16.0.1/");
        assert_eq!(web("172.31.255.1:3000"), "http://172.31.255.1:3000/");
        // Hors plages privées : une adresse publique ou un nom restent en https.
        assert_eq!(web("172.15.0.1"), "https://172.15.0.1/");
        assert_eq!(web("172.32.0.1"), "https://172.32.0.1/");
        assert_eq!(web("8.8.8.8"), "https://8.8.8.8/");
        assert_eq!(web("notlocalhost.dev"), "https://notlocalhost.dev/");
    }

    #[test]
    fn local_documents_are_html_and_pdf_whatever_the_case() {
        assert!(is_local_document(Path::new("a/index.HTML")));
        assert!(is_local_document(Path::new("r.htm")));
        assert!(is_local_document(Path::new("r.Pdf")));
        assert!(!is_local_document(Path::new("main.rs")));
        assert!(!is_local_document(Path::new("pdf")));
    }
}
