//! Les primitives d'accès du compagnon : codes d'appairage, jetons d'appareil, cookie, origines admises. Rien ici ne
//! touche au disque ni au réseau ; les horloges sont passées en argument pour que les délais se testent sans attendre.

use std::ffi::OsStr;
use std::time::{Duration, Instant};

use rand::RngCore;
use sha2::{Digest, Sha256};

/// Le nom du cookie d'appareil.
pub const COOKIE_NAME: &str = "idema_companion";

/// Le cookie d'appareil : 400 jours, borné à `/idema-api` (les cookies ignorent le port, et les autres services de
/// la machine partagent le nom d'hôte).
pub fn cookie_header(token_b64url: &str) -> String {
    format!(
        "{COOKIE_NAME}={token_b64url}; HttpOnly; Secure; SameSite=Strict; Path=/idema-api; Max-Age=34560000"
    )
}

/// Un code d'appairage vaut 10 minutes.
const CODE_LIFETIME: Duration = Duration::from_secs(10 * 60);
/// Au bout de ce nombre d'essais faux, le code est invalidé.
const MAX_WRONG_TRIES: u8 = 5;

/// Les codes d'appairage : au plus un seul valide à la fois.
#[derive(Debug, Default)]
pub struct PairingCodes {
    current: Option<Pending>,
}

#[derive(Debug)]
struct Pending {
    code: String,
    deadline: Instant,
    wrong_tries: u8,
}

/// Ce que donne un essai de code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairOutcome {
    Accepted,
    Wrong,
    Expired,
    Locked,
    NoCode,
}

impl PairingCodes {
    /// Émet un code à 6 chiffres et rend l'instant où il cesse de valoir ; le précédent est remplacé.
    pub fn issue(&mut self, now: Instant, rng: &mut impl RngCore) -> (String, Instant) {
        let deadline = now + CODE_LIFETIME;
        let code = format!("{:06}", uniform_below(rng, 1_000_000));
        self.current = Some(Pending {
            code: code.clone(),
            deadline,
            wrong_tries: 0,
        });
        (code, deadline)
    }

    /// Annule le code en cours.
    pub fn cancel(&mut self) {
        self.current = None;
    }

    /// Essaie un code : bon, il est consommé (usage unique) ; faux, il compte, et au cinquième le code est invalidé.
    pub fn verify(&mut self, code: &str, now: Instant) -> PairOutcome {
        let Some(pending) = &mut self.current else {
            return PairOutcome::NoCode;
        };
        if now >= pending.deadline {
            self.current = None;
            return PairOutcome::Expired;
        }
        if pending.wrong_tries >= MAX_WRONG_TRIES {
            return PairOutcome::Locked;
        }
        if constant_time_eq(code.as_bytes(), pending.code.as_bytes()) {
            self.current = None;
            PairOutcome::Accepted
        } else {
            pending.wrong_tries += 1;
            PairOutcome::Wrong
        }
    }

    /// Le code encore bon à `now` et son échéance ; `None` s'il n'y en a pas, s'il a expiré ou s'il est invalidé.
    pub fn pending(&self, now: Instant) -> Option<(&str, Instant)> {
        self.current
            .as_ref()
            .filter(|pending| now < pending.deadline && pending.wrong_tries < MAX_WRONG_TRIES)
            .map(|pending| (pending.code.as_str(), pending.deadline))
    }

    /// L'échéance du code en cours, bon ou non : à cet instant il n'y a plus rien à garder.
    pub fn deadline(&self) -> Option<Instant> {
        self.current.as_ref().map(|pending| pending.deadline)
    }

    /// Oublie le code s'il a expiré.
    pub fn expire(&mut self, now: Instant) {
        if self
            .current
            .as_ref()
            .is_some_and(|pending| now >= pending.deadline)
        {
            self.current = None;
        }
    }
}

/// Un entier uniforme dans `0..bound`, sans biais de modulo : les tirages du haut de la plage 32 bits sont refaits.
fn uniform_below(rng: &mut impl RngCore, bound: u32) -> u32 {
    let limit = u32::MAX - u32::MAX % bound;
    loop {
        let value = rng.next_u32();
        if value < limit {
            return value % bound;
        }
    }
}

/// Égalité en temps constant pour des chaînes de même longueur (le code ne fuit pas par le temps de la comparaison).
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

/// Un jeton d'appareil : 32 octets du générateur cryptographique.
pub fn new_token(rng: &mut impl RngCore) -> [u8; 32] {
    let mut token = [0u8; 32];
    rng.fill_bytes(&mut token);
    token
}

/// L'empreinte d'un jeton (SHA-256, en hexadécimal) : la seule trace gardée sur disque.
pub fn token_hash(token: &[u8]) -> String {
    use std::fmt::Write;
    Sha256::digest(token)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Un en-tête `Origin` est-il admis ? Absent, oui (un client qui n'est pas un navigateur n'en envoie pas, et sans
/// cookie il n'obtient rien). Présent : l'origine de l'adresse tailnet connue, ou à défaut de toute adresse connue
/// n'importe quelle origine `https://*.ts.net` ; et toujours les origines locales du port.
pub fn origin_allowed(origin: Option<&str>, known: Option<&str>, port: u16) -> bool {
    let Some(origin) = origin else {
        return true;
    };
    if origin == format!("http://127.0.0.1:{port}") || origin == format!("http://localhost:{port}")
    {
        return true;
    }
    match known.and_then(origin_of) {
        Some(known) => origin == known,
        None => is_tailnet_origin(origin),
    }
}

/// L'origine (`schéma://hôte[:port]`) d'une adresse, telle qu'un navigateur l'écrit dans `Origin`.
fn origin_of(address: &str) -> Option<String> {
    let origin = url::Url::parse(address).ok()?.origin();
    origin.is_tuple().then(|| origin.ascii_serialization())
}

/// Une origine `https://<hôte>.ts.net[:port]` écrite sous sa forme canonique (ni chemin, ni identifiants).
fn is_tailnet_origin(origin: &str) -> bool {
    let Ok(parsed) = url::Url::parse(origin) else {
        return false;
    };
    parsed.scheme() == "https"
        && parsed
            .host_str()
            .is_some_and(|host| host.ends_with(".ts.net"))
        && parsed.origin().ascii_serialization() == origin
}

/// Les routes et réglages de test n'existent que si `IDEMA_HOME` (instance isolée) ET `IDEMA_COMPANION_TEST_ROUTES=1`
/// sont posés. Seul le lanceur des tests d'acceptation pose la seconde.
pub fn test_routes_enabled(idema_home: Option<&OsStr>, flag: Option<&OsStr>) -> bool {
    idema_home.is_some_and(|home| !home.is_empty()) && flag == Some(OsStr::new("1"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::companion::test_support::SeededRng;

    #[test]
    fn a_code_is_six_digits_and_expires_after_ten_minutes() {
        let mut codes = PairingCodes::default();
        let start = Instant::now();
        let (code, deadline) = codes.issue(start, &mut SeededRng::new(7));
        assert_eq!(code.len(), 6);
        assert!(code.bytes().all(|byte| byte.is_ascii_digit()), "{code}");
        assert_eq!(deadline, start + Duration::from_secs(600));

        // Un second code, émis à son tour : 9 min 59 s plus tard il est encore bon.
        let (second, _) = codes.issue(start, &mut SeededRng::new(8));
        assert_eq!(
            codes.verify(&second, start + Duration::from_secs(599)),
            PairOutcome::Accepted
        );

        // 10 minutes pile : expiré, et l'échéance a fait disparaître le code.
        let (third, _) = codes.issue(start, &mut SeededRng::new(9));
        assert_eq!(
            codes.verify(&third, start + Duration::from_secs(600)),
            PairOutcome::Expired
        );
        assert_eq!(
            codes.verify(&third, start + Duration::from_secs(601)),
            PairOutcome::NoCode
        );
    }

    #[test]
    fn issuing_a_code_replaces_the_previous_one() {
        let mut codes = PairingCodes::default();
        let now = Instant::now();
        let (first, _) = codes.issue(now, &mut SeededRng::new(1));
        let (second, _) = codes.issue(now, &mut SeededRng::new(2));
        assert_ne!(first, second);
        assert_eq!(codes.verify(&first, now), PairOutcome::Wrong);
        assert_eq!(codes.verify(&second, now), PairOutcome::Accepted);
    }

    #[test]
    fn a_code_is_single_use() {
        let mut codes = PairingCodes::default();
        let now = Instant::now();
        let (code, _) = codes.issue(now, &mut SeededRng::new(7));
        assert_eq!(codes.verify(&code, now), PairOutcome::Accepted);
        assert_eq!(codes.verify(&code, now), PairOutcome::NoCode);
        assert!(codes.pending(now).is_none());
    }

    #[test]
    fn five_wrong_codes_lock_the_code() {
        let mut codes = PairingCodes::default();
        let now = Instant::now();
        let (code, _) = codes.issue(now, &mut SeededRng::new(7));
        let wrong = if code == "000000" { "111111" } else { "000000" };
        for _ in 0..5 {
            assert_eq!(codes.verify(wrong, now), PairOutcome::Wrong);
        }
        assert_eq!(codes.verify(&code, now), PairOutcome::Locked);
        assert!(codes.pending(now).is_none(), "a locked code is not shown");
    }

    #[test]
    fn a_cancelled_code_is_gone() {
        let mut codes = PairingCodes::default();
        let now = Instant::now();
        assert_eq!(codes.verify("123456", now), PairOutcome::NoCode);
        let (code, _) = codes.issue(now, &mut SeededRng::new(7));
        assert!(codes.pending(now).is_some());
        codes.cancel();
        assert_eq!(codes.verify(&code, now), PairOutcome::NoCode);
        assert!(codes.deadline().is_none());
    }

    #[test]
    fn an_expired_code_is_dropped_by_expire_only_once_due() {
        let mut codes = PairingCodes::default();
        let now = Instant::now();
        let (_, deadline) = codes.issue(now, &mut SeededRng::new(7));
        codes.expire(deadline - Duration::from_secs(1));
        assert_eq!(codes.deadline(), Some(deadline));
        codes.expire(deadline);
        assert!(codes.deadline().is_none());
    }

    #[test]
    fn codes_cover_the_whole_range_with_leading_zeros() {
        let mut codes = PairingCodes::default();
        let now = Instant::now();
        // Un générateur qui ne rend que des petites valeurs : le code garde ses zéros de tête.
        struct Fixed(u32);
        impl RngCore for Fixed {
            fn next_u32(&mut self) -> u32 {
                self.0
            }
            fn next_u64(&mut self) -> u64 {
                u64::from(self.0)
            }
            fn fill_bytes(&mut self, dest: &mut [u8]) {
                dest.fill(self.0 as u8);
            }
            fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand::Error> {
                self.fill_bytes(dest);
                Ok(())
            }
        }
        assert_eq!(codes.issue(now, &mut Fixed(42)).0, "000042");
        // Une valeur du haut de la plage 32 bits (biais du modulo) est tirée de nouveau, pas repliée.
        struct Biased(Vec<u32>);
        impl RngCore for Biased {
            fn next_u32(&mut self) -> u32 {
                self.0.remove(0)
            }
            fn next_u64(&mut self) -> u64 {
                u64::from(self.next_u32())
            }
            fn fill_bytes(&mut self, _: &mut [u8]) {}
            fn try_fill_bytes(&mut self, _: &mut [u8]) -> Result<(), rand::Error> {
                Ok(())
            }
        }
        assert_eq!(
            codes
                .issue(now, &mut Biased(vec![u32::MAX, 4_294_000_000, 123]))
                .0,
            "000123"
        );
    }

    #[test]
    fn the_cookie_is_bound_to_the_api_path() {
        assert_eq!(
            cookie_header("abc"),
            "idema_companion=abc; HttpOnly; Secure; SameSite=Strict; Path=/idema-api; Max-Age=34560000"
        );
    }

    #[test]
    fn tokens_are_random_and_only_their_hash_is_kept() {
        let first = new_token(&mut SeededRng::new(1));
        let second = new_token(&mut SeededRng::new(2));
        assert_ne!(first, second);
        let hash = token_hash(&first);
        assert_eq!(hash.len(), 64);
        assert!(hash.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(hash, token_hash(&second));
        // Empreinte connue : SHA-256 de la chaîne vide.
        assert_eq!(
            token_hash(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn origin_rules() {
        let known = Some("https://forge.example.ts.net:8455");
        assert!(origin_allowed(
            Some("https://forge.example.ts.net:8455"),
            known,
            3910
        ));
        assert!(!origin_allowed(Some("https://evil.example"), known, 3910));
        assert!(
            !origin_allowed(Some("https://x.example.ts.net:8455"), known, 3910),
            "with a known address, another tailnet host is refused"
        );
        assert!(origin_allowed(None, known, 3910));
        assert!(origin_allowed(
            Some("https://x.example.ts.net:8455"),
            None,
            3910
        ));
        assert!(!origin_allowed(Some("https://evil.example"), None, 3910));
        assert!(origin_allowed(Some("http://127.0.0.1:3910"), known, 3910));
        assert!(origin_allowed(Some("http://localhost:3910"), None, 3910));
        assert!(!origin_allowed(Some("http://127.0.0.1:3911"), None, 3910));
        // La forme saisie dans `public_url` (barre finale, chemin) désigne la même origine.
        assert!(origin_allowed(
            Some("https://forge.example.ts.net:8455"),
            Some("https://forge.example.ts.net:8455/"),
            3910
        ));
    }

    #[test]
    fn origin_rules_refuse_look_alikes() {
        for origin in [
            "https://evil.example/.ts.net",
            "https://evil.example#.ts.net",
            "https://x.ts.net.evil.example",
            "https://user@x.example.ts.net",
            "http://x.example.ts.net:8455",
            "https://ts.net",
            "null",
            "",
        ] {
            assert!(!origin_allowed(Some(origin), None, 3910), "{origin}");
        }
    }

    #[test]
    fn test_routes_need_both_variables() {
        let home = OsStr::new("/tmp/home");
        let on = OsStr::new("1");
        assert!(test_routes_enabled(Some(home), Some(on)));
        assert!(!test_routes_enabled(None, Some(on)));
        assert!(!test_routes_enabled(Some(OsStr::new("")), Some(on)));
        assert!(!test_routes_enabled(Some(home), None));
        assert!(!test_routes_enabled(Some(home), Some(OsStr::new("0"))));
        assert!(!test_routes_enabled(Some(home), Some(OsStr::new("true"))));
        assert!(!test_routes_enabled(None, None));
    }
}
