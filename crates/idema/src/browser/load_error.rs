//! Le message d'un chargement raté (spec P4 §6) : la cause en une ligne, en français, d'après le code d'erreur
//! `NSURLError` que WebKit rapporte, et l'offre de démarrer le serveur dont c'est le port. Aucune fenêtre ici : de la
//! correspondance de codes et d'états, testable seule.

use crate::servers::registry::{RowKind, RowState, ServerKey, ServerRow};

/// Ce que l'onglet dit quand sa page ne charge pas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadError {
    pub message: String,
}

/// `NSURLErrorCannotConnectToHost`.
const CONNECTION_REFUSED: isize = -1004;
/// `NSURLErrorCannotFindHost`.
const HOST_NOT_FOUND: isize = -1003;
/// `NSURLErrorSecureConnectionFailed` (-1200) à `NSURLErrorClientCertificateRequired` (-1206) : la connexion sécurisée
/// ou son certificat est refusé.
const CERTIFICATE_REFUSED: std::ops::RangeInclusive<isize> = -1206..=-1200;

/// La ligne d'erreur d'un code. Les trois causes qu'on sait dire nomment l'hôte ; sans hôte (rare : une erreur qui ne
/// cite aucune adresse), ou pour tout autre code, la description de WebKit.
pub fn load_error(code: isize, host: Option<&str>, description: &str) -> LoadError {
    let message = match (code, host) {
        (CONNECTION_REFUSED, Some(host)) => format!("Connexion refusée : {host}"),
        (HOST_NOT_FOUND, Some(host)) => format!("Hôte introuvable : {host}"),
        (code, Some(host)) if CERTIFICATE_REFUSED.contains(&code) => {
            format!("Certificat refusé : {host}")
        }
        _ => format!("La page n'a pas pu se charger : {description}"),
    };
    LoadError { message }
}

/// L'hôte d'une adresse, avec son port quand elle en porte un (`localhost:8819`, `[::1]:3000`) : de quoi dire quelle
/// connexion a été refusée. `None` pour une adresse sans hôte.
pub fn host_with_port(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    Some(match parsed.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    })
}

/// Ce que l'onglet propose quand l'adresse qui n'a pas chargé est le port d'une entrée de configuration à l'arrêt
/// (spec §6, AC-81) : nommer le serveur et offrir de le démarrer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartOffer {
    pub message: String,
    /// L'entrée à démarrer. `None` : le serveur démarre déjà, le message le dit et il n'y a rien à cliquer.
    pub key: Option<ServerKey>,
}

/// L'offre de démarrage pour la ligne de configuration que l'adresse désigne (`Servers::row_for_url`). Une ligne en
/// marche n'en fait pas (la page a échoué pour une autre raison), pas plus qu'une adresse sans ligne.
pub fn start_offer(row: Option<&ServerRow>) -> Option<StartOffer> {
    let row = row?;
    let RowKind::Config(key) = &row.kind else {
        return None;
    };
    match row.state {
        RowState::Stopped | RowState::Failed => Some(StartOffer {
            message: format!("Le serveur « {} » ne tourne pas", row.name),
            key: Some(key.clone()),
        }),
        RowState::Starting => Some(StartOffer {
            message: format!("Le serveur « {} » démarre", row.name),
            key: None,
        }),
        RowState::Running { .. } | RowState::StopRefused { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn entry(state: RowState, kind: RowKind) -> ServerRow {
        ServerRow {
            kind,
            project: PathBuf::from("/work/alpha"),
            name: "web".to_owned(),
            command: "python3 -m http.server 8811".to_owned(),
            port: Some(8811),
            url: "http://localhost:8811".to_owned(),
            state,
            origin: None,
            listener_pid: None,
            output: None,
            session_alive: false,
            extra_ports: Vec::new(),
            failure: None,
        }
    }

    fn config() -> RowKind {
        RowKind::Config(ServerKey {
            project: PathBuf::from("/work/alpha"),
            name: "web".to_owned(),
        })
    }

    #[test]
    fn the_refused_connection_names_the_port_of_the_address() {
        assert_eq!(
            host_with_port("http://localhost:8819/").as_deref(),
            Some("localhost:8819")
        );
        assert_eq!(
            host_with_port("https://vercel.com/x?y=1").as_deref(),
            Some("vercel.com")
        );
        assert_eq!(
            host_with_port("http://[::1]:3000/").as_deref(),
            Some("[::1]:3000")
        );
        assert_eq!(host_with_port("pas une adresse"), None);
        let label = host_with_port("http://localhost:8819/").unwrap();
        assert_eq!(
            load_error(-1004, Some(&label), "x").message,
            "Connexion refusée : localhost:8819"
        );
    }

    #[test]
    fn a_stopped_entry_is_offered_to_start() {
        for state in [RowState::Stopped, RowState::Failed] {
            let offer = start_offer(Some(&entry(state, config()))).expect("an offer");
            assert_eq!(offer.message, "Le serveur « web » ne tourne pas");
            assert!(offer.key.is_some());
        }
    }

    #[test]
    fn a_starting_entry_says_so_and_offers_nothing_to_click() {
        let offer = start_offer(Some(&entry(RowState::Starting, config()))).expect("an offer");
        assert_eq!(offer.message, "Le serveur « web » démarre");
        assert_eq!(offer.key, None);
    }

    #[test]
    fn no_offer_for_a_running_server_a_detected_one_or_no_line() {
        let running = RowState::Running { pid: 1, port: 8811 };
        assert_eq!(start_offer(Some(&entry(running, config()))), None);
        let detected = RowKind::Detected { pid: 1, port: 8811 };
        assert_eq!(start_offer(Some(&entry(RowState::Stopped, detected))), None);
        assert_eq!(start_offer(None), None);
    }

    #[test]
    fn load_errors_name_their_cause_in_french() {
        assert_eq!(
            load_error(-1004, Some("localhost"), "x").message,
            "Connexion refusée : localhost"
        );
        assert_eq!(
            load_error(-1003, Some("nope.invalid"), "x").message,
            "Hôte introuvable : nope.invalid"
        );
        assert_eq!(
            load_error(-1202, Some("self-signed.dev"), "x").message,
            "Certificat refusé : self-signed.dev"
        );
        assert_eq!(
            load_error(-1001, Some("a.dev"), "The request timed out.").message,
            "La page n'a pas pu se charger : The request timed out."
        );
    }

    #[test]
    fn every_secure_connection_code_is_a_refused_certificate() {
        for code in -1206..=-1200 {
            assert_eq!(
                load_error(code, Some("a.dev"), "x").message,
                "Certificat refusé : a.dev",
                "code {code}"
            );
        }
        // Les codes voisins restent des échecs génériques.
        for code in [-1199, -1207] {
            assert_eq!(
                load_error(code, Some("a.dev"), "boum").message,
                "La page n'a pas pu se charger : boum",
                "code {code}"
            );
        }
    }

    #[test]
    fn a_known_cause_without_a_host_falls_back_to_the_description() {
        assert_eq!(
            load_error(-1004, None, "Could not connect").message,
            "La page n'a pas pu se charger : Could not connect"
        );
    }
}
