//! Chiffrement et signature des notifications Web Push : RFC 8291 (contenu `aes128gcm`) et RFC 8292 (VAPID, ES256).
//! Module pur, sans réseau : l'envoi vient après. Le sel et la clé éphémère sont injectables pour que l'annexe A de
//! la RFC se rejoue à l'octet.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aes_gcm::aead::{Aead, KeyInit};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use hkdf::Hkdf;
use p256::ecdsa::SigningKey;
use p256::ecdsa::signature::Signer;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use rand::RngCore;
use rand::rngs::OsRng;
use sha2::Sha256;

use super::devices::Subscription;

/// Le contact déclaré au service push dans la revendication `sub` du jeton VAPID.
pub const VAPID_SUB: &str = "https://github.com/eRom/erom-IDEMA";

/// Taille d'enregistrement annoncée dans l'en-tête (RFC 8291 §4) : un message tient dans un seul enregistrement.
const RECORD_SIZE: u32 = 4096;
/// Le plus long texte qu'un service push doit accepter (RFC 8291 §4) : 4096 - 86 - 1 - 16.
const MAX_PLAINTEXT: usize = 3993;
/// Plus petit enregistrement qui garde de la place pour la balise GCM (16) et le délimiteur (1) : RFC 8188 §2.
const MIN_RECORD_SIZE: u32 = 18;
/// Longueur de l'en-tête : sel (16) + taille d'enregistrement (4) + longueur de l'identifiant (1) + clé publique (65).
const HEADER_LEN: usize = 16 + 4 + 1 + 65;
/// Un jeton VAPID est servi pendant 12 h au plus (spec §3.7).
const TOKEN_REUSE: Duration = Duration::from_secs(12 * 3600);
/// Durée de vie annoncée : sous les 24 h du RFC 8292 avec de la marge pour les horloges, et au-delà des 12 h de
/// réutilisation pour que le dernier envoi servi par le cache soit encore valide.
const TOKEN_LIFETIME: Duration = Duration::from_secs(18 * 3600);

#[derive(Debug, PartialEq, Eq)]
pub enum PushError {
    /// L'abonnement n'a pas la forme attendue (clé du navigateur ou secret d'authentification).
    BadSubscription(&'static str),
    /// Le texte ne tient pas dans un seul enregistrement.
    PayloadTooLarge,
    /// Taille d'enregistrement trop petite pour porter la balise et le délimiteur.
    BadRecordSize,
    /// Dérivation ou chiffrement refusé par la bibliothèque.
    Crypto,
}

impl std::fmt::Display for PushError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadSubscription(reason) => write!(f, "bad subscription: {reason}"),
            Self::PayloadTooLarge => f.write_str("payload does not fit one record"),
            Self::BadRecordSize => f.write_str("record size too small"),
            Self::Crypto => f.write_str("encryption failed"),
        }
    }
}

impl std::error::Error for PushError {}

/// Les clés du navigateur d'un appareil : point P-256 non compressé et secret d'authentification de 16 octets.
pub struct UaKeys {
    pub p256dh: [u8; 65],
    pub auth: [u8; 16],
}

impl UaKeys {
    /// Décode l'abonnement (base64url sans remplissage) et vérifie que `p256dh` est bien un point de la courbe : une
    /// clé fausse ferait échouer chaque envoi, autant la refuser à l'enregistrement.
    pub fn from_subscription(sub: &Subscription) -> Result<Self, PushError> {
        let p256dh: [u8; 65] = B64
            .decode(&sub.p256dh)
            .map_err(|_| PushError::BadSubscription("p256dh is not base64url"))?
            .try_into()
            .map_err(|_| PushError::BadSubscription("p256dh is not 65 bytes"))?;
        p256::PublicKey::from_sec1_bytes(&p256dh)
            .map_err(|_| PushError::BadSubscription("p256dh is not a P-256 point"))?;
        let auth: [u8; 16] = B64
            .decode(&sub.auth)
            .map_err(|_| PushError::BadSubscription("auth is not base64url"))?
            .try_into()
            .map_err(|_| PushError::BadSubscription("auth is not 16 bytes"))?;
        Ok(Self { p256dh, auth })
    }
}

/// Chiffre un message en `aes128gcm` (RFC 8291) avec un sel et une clé d'expéditeur donnés ; un seul enregistrement,
/// terminé par le délimiteur 0x02. Rend l'en-tête de 86 octets suivi du texte chiffré.
pub fn encrypt_with(
    plaintext: &[u8],
    ua: &UaKeys,
    salt: [u8; 16],
    sender: &p256::SecretKey,
    record_size: u32,
) -> Result<Vec<u8>, PushError> {
    if record_size < MIN_RECORD_SIZE {
        return Err(PushError::BadRecordSize);
    }
    // texte + délimiteur + balise GCM doivent tenir dans l'enregistrement.
    if plaintext.len() + 1 + 16 > record_size as usize {
        return Err(PushError::PayloadTooLarge);
    }
    let ua_public = p256::PublicKey::from_sec1_bytes(&ua.p256dh)
        .map_err(|_| PushError::BadSubscription("p256dh is not a P-256 point"))?;
    let as_public = sender.public_key().to_encoded_point(false);
    let shared = p256::ecdh::diffie_hellman(sender.to_nonzero_scalar(), ua_public.as_affine());

    // RFC 8291 §3.4 : IKM = HKDF(auth, ecdh, "WebPush: info\0" || ua_public || as_public), puis la clé et le nonce
    // du contenu sortent d'un second HKDF salé par `salt` (RFC 8188 §2.2-2.3).
    let mut key_info = Vec::with_capacity(14 + 65 + 65);
    key_info.extend_from_slice(b"WebPush: info\0");
    key_info.extend_from_slice(&ua.p256dh);
    key_info.extend_from_slice(as_public.as_bytes());
    let mut ikm = [0u8; 32];
    Hkdf::<Sha256>::new(Some(&ua.auth), shared.raw_secret_bytes())
        .expand(&key_info, &mut ikm)
        .map_err(|_| PushError::Crypto)?;
    let prk = Hkdf::<Sha256>::new(Some(&salt), &ikm);
    let mut content_key = [0u8; 16];
    prk.expand(b"Content-Encoding: aes128gcm\0", &mut content_key)
        .map_err(|_| PushError::Crypto)?;
    let mut nonce = [0u8; 12];
    prk.expand(b"Content-Encoding: nonce\0", &mut nonce)
        .map_err(|_| PushError::Crypto)?;

    let mut record = Vec::with_capacity(plaintext.len() + 1);
    record.extend_from_slice(plaintext);
    record.push(0x02);
    let ciphertext = aes_gcm::Aes128Gcm::new_from_slice(&content_key)
        .map_err(|_| PushError::Crypto)?
        .encrypt(aes_gcm::Nonce::from_slice(&nonce), record.as_slice())
        .map_err(|_| PushError::Crypto)?;

    let mut body = Vec::with_capacity(HEADER_LEN + ciphertext.len());
    body.extend_from_slice(&salt);
    body.extend_from_slice(&record_size.to_be_bytes());
    body.push(65);
    body.extend_from_slice(as_public.as_bytes());
    body.extend_from_slice(&ciphertext);
    Ok(body)
}

/// Chiffre pour un appareil avec un sel et une clé éphémère neufs ; chaque message a les siens.
pub fn encrypt(plaintext: &[u8], ua: &UaKeys) -> Result<Vec<u8>, PushError> {
    // RFC 8291 §4 : un service push n'a pas à accepter plus de 4096 octets de corps, soit 3993 octets de texte une fois
    // retirés l'en-tête (86), le délimiteur (1) et la balise GCM (16).
    if plaintext.len() > MAX_PLAINTEXT {
        return Err(PushError::PayloadTooLarge);
    }
    let mut salt = [0u8; 16];
    OsRng.fill_bytes(&mut salt);
    let sender = p256::SecretKey::random(&mut OsRng);
    encrypt_with(plaintext, ua, salt, &sender, RECORD_SIZE)
}

/// La clé de signature VAPID du Mac et les en-têtes `Authorization` déjà signés, par origine de service push.
pub struct Vapid {
    key: SigningKey,
    public: String,
    /// Origine du service push -> (instant de signature, en-tête).
    cache: HashMap<String, (SystemTime, String)>,
}

impl Vapid {
    /// Lit `<dir>/companion/vapid.key` (32 octets du scalaire, 0600) ou le crée. Un fichier illisible ou abîmé est une
    /// erreur : repartir d'une clé neuve invaliderait en silence l'abonnement de chaque téléphone.
    pub fn load_or_create(dir: &Path) -> io::Result<Self> {
        let folder = dir.join("companion");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&folder)
            .or_else(|error| {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    Ok(())
                } else {
                    Err(error)
                }
            })?;
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o700))?;
        let path = folder.join("vapid.key");
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                publish_new_key(&folder, &path)?;
                // Relue du fichier, jamais gardée en mémoire : si un autre appel a publié la sienne en premier, les
                // deux lisent la même.
                fs::read(&path)?
            }
            Err(error) => return Err(error),
        };
        // Un fichier laissé avec des droits plus larges (restauré, copié) est resserré avant tout usage.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        let secret = p256::SecretKey::from_slice(&bytes).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "vapid.key is not a P-256 key")
        })?;
        let key = SigningKey::from(secret);
        let public = B64.encode(key.verifying_key().to_encoded_point(false).as_bytes());
        Ok(Self {
            key,
            public,
            cache: HashMap::new(),
        })
    }

    /// Le point public non compressé, en base64url (87 caractères) : la valeur de `applicationServerKey` côté PWA.
    pub fn public_key_b64url(&self) -> String {
        self.public.clone()
    }

    /// L'en-tête `Authorization: vapid t=<jwt>, k=<clé>` pour l'origine de `endpoint`. Le jeton signé est gardé 12 h
    /// par origine. Un `endpoint` sans origine HTTP(S) est signé tel quel et le service push le refusera : la forme
    /// de l'abonnement se contrôle en amont, à son enregistrement.
    pub fn authorization(&mut self, endpoint: &str, now: SystemTime) -> String {
        let audience = push_origin(endpoint).unwrap_or_else(|| endpoint.to_string());
        if let Some((issued, header)) = self.cache.get(&audience)
            && now
                .duration_since(*issued)
                .is_ok_and(|age| age < TOKEN_REUSE)
        {
            return header.clone();
        }
        let expires = now
            .checked_add(TOKEN_LIFETIME)
            .unwrap_or(now)
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let header = B64.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = B64.encode(
            serde_json::json!({ "aud": audience, "exp": expires, "sub": VAPID_SUB }).to_string(),
        );
        let signed = format!("{header}.{claims}");
        let signature: p256::ecdsa::Signature = self.key.sign(signed.as_bytes());
        let value = format!(
            "vapid t={signed}.{}, k={}",
            B64.encode(signature.to_bytes()),
            self.public
        );
        // Les origines expirées ne s'accumulent pas : on ne garde que ce qui sert encore.
        self.cache.retain(|_, (issued, _)| {
            now.duration_since(*issued)
                .is_ok_and(|age| age < TOKEN_REUSE)
        });
        self.cache.insert(audience, (now, value.clone()));
        value
    }
}

/// Crée `vapid.key` d'un coup : la clé est écrite et synchronisée dans un fichier voisin 0600 au nom unique (un reste
/// d'un process mort ne gêne jamais), puis liée sous son nom définitif. Le lien échoue si la cible existe : le premier
/// arrivé gagne et l'appel suivant lit sa clé. Jamais de `vapid.key` vide ou à demi écrit, même si le process meurt
/// ou si le disque se remplit en route.
fn publish_new_key(folder: &Path, path: &Path) -> io::Result<()> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let temporary = folder.join(format!(
        "vapid.key.{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let linked = (|| {
        let bytes = p256::SecretKey::random(&mut OsRng).to_bytes();
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        match fs::hard_link(&temporary, path) {
            Err(error) if error.kind() != io::ErrorKind::AlreadyExists => Err(error),
            _ => Ok(()),
        }
    })();
    // Le fichier voisin ne doit pas rester : l'échec d'origine, s'il y en a un, est celui qu'on rend.
    let _ = fs::remove_file(&temporary);
    linked
}

/// L'origine (`https://hôte[:port]`) d'un point d'accès push, ou `None` si ce n'est pas une URL HTTP(S).
fn push_origin(endpoint: &str) -> Option<String> {
    let url = url::Url::parse(endpoint).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.origin().ascii_serialization())
}

/// Les en-têtes de l'envoi : durée de vie d'une heure, urgence haute, `Topic` pour que le service push remplace une
/// notification pas encore livrée.
pub fn headers(topic: &str) -> [(&'static str, String); 5] {
    [
        ("TTL", "3600".to_string()),
        ("Urgency", "high".to_string()),
        ("Topic", topic.to_string()),
        ("Content-Encoding", "aes128gcm".to_string()),
        ("Content-Type", "application/octet-stream".to_string()),
    ]
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
    use p256::ecdsa::signature::Verifier;
    use p256::ecdsa::{Signature, VerifyingKey};
    use rand::RngCore;
    use rand::rngs::OsRng;

    use super::*;
    use crate::companion::test_support::TempDir;

    fn decode(text: &str) -> Vec<u8> {
        B64.decode(text).unwrap()
    }

    fn secret(text: &str) -> p256::SecretKey {
        p256::SecretKey::from_slice(&decode(text)).unwrap()
    }

    fn subscription(ua_public: &[u8], auth: &[u8]) -> Subscription {
        Subscription {
            endpoint: "https://web.push.apple.com/QXX".into(),
            p256dh: B64.encode(ua_public),
            auth: B64.encode(auth),
        }
    }

    /// Le déchiffrement du récepteur (RFC 8291 §3.4, côté agent utilisateur) : part de la clé privée du téléphone et de
    /// la clé publique de l'expéditeur lue dans l'en-tête, donc un chemin différent de celui de `encrypt_with`.
    fn decrypt_as_user_agent(
        body: &[u8],
        ua_private: &p256::SecretKey,
        auth: &[u8; 16],
    ) -> Vec<u8> {
        use aes_gcm::aead::{Aead, KeyInit};
        use hkdf::Hkdf;
        use p256::elliptic_curve::sec1::ToEncodedPoint;
        use sha2::Sha256;

        let salt = &body[..16];
        let id_len = body[20] as usize;
        assert_eq!(id_len, 65);
        let as_public = &body[21..21 + id_len];
        let ciphertext = &body[21 + id_len..];
        let sender = p256::PublicKey::from_sec1_bytes(as_public).unwrap();
        let shared = p256::ecdh::diffie_hellman(ua_private.to_nonzero_scalar(), sender.as_affine());
        let ua_public = ua_private.public_key().to_encoded_point(false);
        let mut key_info = b"WebPush: info\0".to_vec();
        key_info.extend_from_slice(ua_public.as_bytes());
        key_info.extend_from_slice(as_public);
        let mut ikm = [0u8; 32];
        Hkdf::<Sha256>::new(Some(auth), shared.raw_secret_bytes())
            .expand(&key_info, &mut ikm)
            .unwrap();
        let prk = Hkdf::<Sha256>::new(Some(salt), &ikm);
        let mut cek = [0u8; 16];
        prk.expand(b"Content-Encoding: aes128gcm\0", &mut cek)
            .unwrap();
        let mut nonce = [0u8; 12];
        prk.expand(b"Content-Encoding: nonce\0", &mut nonce)
            .unwrap();
        let mut padded = aes_gcm::Aes128Gcm::new_from_slice(&cek)
            .unwrap()
            .decrypt(aes_gcm::Nonce::from_slice(&nonce), ciphertext)
            .unwrap();
        while padded.last() == Some(&0) {
            padded.pop();
        }
        assert_eq!(padded.pop(), Some(0x02), "last record delimiter");
        padded
    }

    #[test]
    fn rfc8291_appendix_a_is_reproduced() {
        let plaintext = decode("V2hlbiBJIGdyb3cgdXAsIEkgd2FudCB0byBiZSBhIHdhdGVybWVsb24");
        let ua_public = decode(
            "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
        );
        let auth = decode("BTBZMqHH6r4Tts7J_aSIgg");
        let ua = UaKeys::from_subscription(&subscription(&ua_public, &auth)).unwrap();
        let salt: [u8; 16] = decode("DGv6ra1nlYgDCS1FRnbzlw").try_into().unwrap();
        let sender = secret("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw");
        let expected = decode(
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN",
        );
        assert_eq!(expected.len(), 144);

        let body = encrypt_with(&plaintext, &ua, salt, &sender, 4096).unwrap();

        assert_eq!(body, expected);
        // Et le récepteur de l'annexe retrouve le texte : le vecteur n'est pas seulement reproduit, il se lit.
        let ua_private = secret("q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94");
        assert_eq!(
            decrypt_as_user_agent(&body, &ua_private, &ua.auth),
            plaintext
        );
    }

    #[test]
    fn a_random_encryption_round_trips() {
        let ua_private = p256::SecretKey::random(&mut OsRng);
        let mut auth = [0u8; 16];
        OsRng.fill_bytes(&mut auth);
        let ua_public = {
            use p256::elliptic_curve::sec1::ToEncodedPoint;
            ua_private.public_key().to_encoded_point(false)
        };
        let ua = UaKeys::from_subscription(&subscription(ua_public.as_bytes(), &auth)).unwrap();
        let mut plaintext = vec![0u8; 3000];
        OsRng.fill_bytes(&mut plaintext);

        let first = encrypt(&plaintext, &ua).unwrap();
        let second = encrypt(&plaintext, &ua).unwrap();

        assert_eq!(decrypt_as_user_agent(&first, &ua_private, &auth), plaintext);
        assert_eq!(
            decrypt_as_user_agent(&second, &ua_private, &auth),
            plaintext
        );
        assert_ne!(first, second, "salt and ephemeral key differ each time");
        assert_eq!(&first[16..20], &4096u32.to_be_bytes());
    }

    #[test]
    fn a_payload_that_does_not_fit_one_record_is_refused() {
        let ua_private = p256::SecretKey::random(&mut OsRng);
        let ua = UaKeys {
            p256dh: {
                use p256::elliptic_curve::sec1::ToEncodedPoint;
                ua_private
                    .public_key()
                    .to_encoded_point(false)
                    .as_bytes()
                    .try_into()
                    .unwrap()
            },
            auth: [7; 16],
        };
        let sender = p256::SecretKey::random(&mut OsRng);
        // Un enregistrement de 4096 octets garde 4096 - 16 (balise) - 1 (délimiteur) octets pour le texte.
        let fits = vec![1u8; 4096 - 16 - 1];
        assert!(encrypt_with(&fits, &ua, [0; 16], &sender, 4096).is_ok());
        let too_long = vec![1u8; 4096 - 16];
        assert_eq!(
            encrypt_with(&too_long, &ua, [0; 16], &sender, 4096),
            Err(PushError::PayloadTooLarge)
        );
        assert_eq!(
            encrypt_with(b"x", &ua, [0; 16], &sender, 17),
            Err(PushError::BadRecordSize)
        );
    }

    #[test]
    fn a_malformed_subscription_is_refused() {
        let ua_private = p256::SecretKey::random(&mut OsRng);
        let valid = {
            use p256::elliptic_curve::sec1::ToEncodedPoint;
            ua_private.public_key().to_encoded_point(false)
        };
        let auth = [9u8; 16];
        assert!(UaKeys::from_subscription(&subscription(valid.as_bytes(), &auth)).is_ok());
        // Trop court, pas un point de la courbe, secret d'authentification de mauvaise taille, base64 invalide.
        assert!(UaKeys::from_subscription(&subscription(&valid.as_bytes()[..64], &auth)).is_err());
        let mut off_curve = valid.as_bytes().to_vec();
        off_curve[64] ^= 1;
        assert!(UaKeys::from_subscription(&subscription(&off_curve, &auth)).is_err());
        assert!(UaKeys::from_subscription(&subscription(valid.as_bytes(), &auth[..15])).is_err());
        let mut bad_text = subscription(valid.as_bytes(), &auth);
        bad_text.auth = "not base64 !".into();
        assert!(UaKeys::from_subscription(&bad_text).is_err());
    }

    fn vapid_in(label: &str) -> (TempDir, Vapid) {
        let home = TempDir::new(label);
        let vapid = Vapid::load_or_create(&home.0).unwrap();
        (home, vapid)
    }

    fn token_of(authorization: &str) -> (String, String) {
        let rest = authorization.strip_prefix("vapid t=").unwrap();
        let (token, key) = rest.split_once(", k=").unwrap();
        (token.to_string(), key.to_string())
    }

    fn at(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn vapid_token_verifies_and_carries_the_claims() {
        let (_home, mut vapid) = vapid_in("vapid-claims");
        let now = at(1_800_000_000);

        let (token, key) = token_of(&vapid.authorization("https://web.push.apple.com/QXX", now));

        assert_eq!(key, vapid.public_key_b64url());
        let public = decode(&key);
        assert_eq!(key.len(), 87);
        assert_eq!((public.len(), public[0]), (65, 0x04));
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header: serde_json::Value = serde_json::from_slice(&decode(parts[0])).unwrap();
        assert_eq!(header["typ"], "JWT");
        assert_eq!(header["alg"], "ES256");
        let claims: serde_json::Value = serde_json::from_slice(&decode(parts[1])).unwrap();
        assert_eq!(claims["aud"], "https://web.push.apple.com");
        assert_eq!(claims["sub"], VAPID_SUB);
        let exp = claims["exp"].as_u64().unwrap();
        assert!(exp > 1_800_000_000, "expires in the future");
        assert!(exp - 1_800_000_000 <= 86_400, "RFC 8292: at most 24 h");
        let signature = Signature::from_slice(&decode(parts[2])).unwrap();
        VerifyingKey::from_sec1_bytes(&public)
            .unwrap()
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .expect("the signature verifies with the published key");
        // Altérer une revendication casse la signature.
        let forged = format!(
            "{}.{}",
            parts[0],
            B64.encode(br#"{"aud":"https://evil.example"}"#)
        );
        assert!(
            VerifyingKey::from_sec1_bytes(&public)
                .unwrap()
                .verify(forged.as_bytes(), &signature)
                .is_err()
        );
    }

    #[test]
    fn vapid_audience_is_the_origin_of_each_endpoint() {
        let (_home, mut vapid) = vapid_in("vapid-aud");
        let now = at(1_800_000_000);
        let audience = |vapid: &mut Vapid, endpoint: &str| {
            let (token, _) = token_of(&vapid.authorization(endpoint, now));
            let claims: serde_json::Value =
                serde_json::from_slice(&decode(token.split('.').nth(1).unwrap())).unwrap();
            claims["aud"].as_str().unwrap().to_string()
        };
        assert_eq!(
            audience(&mut vapid, "https://fcm.googleapis.com/fcm/send/abc?x=1"),
            "https://fcm.googleapis.com"
        );
        assert_eq!(
            audience(
                &mut vapid,
                "https://updates.push.services.mozilla.com:8443/wpush/v2/x"
            ),
            "https://updates.push.services.mozilla.com:8443"
        );
    }

    #[test]
    fn vapid_token_is_reused_for_twelve_hours() {
        let (_home, mut vapid) = vapid_in("vapid-cache");
        let start = at(1_800_000_000);
        let endpoint = "https://web.push.apple.com/QXX";

        let first = vapid.authorization(endpoint, start);
        let almost = vapid.authorization(
            "https://web.push.apple.com/another-device",
            start + Duration::from_secs(12 * 3600 - 60),
        );
        let after = vapid.authorization(endpoint, start + Duration::from_secs(12 * 3600));
        let other_origin = vapid.authorization("https://fcm.googleapis.com/fcm/send/x", start);

        assert_eq!(first, almost, "same origin, 11 h 59: same header");
        assert_ne!(first, after, "12 h: a new token");
        assert_ne!(first, other_origin, "another origin has its own token");
        // Le jeton renouvelé vit assez pour être servi 12 h de plus.
        let (token, _) = token_of(&after);
        let claims: serde_json::Value =
            serde_json::from_slice(&decode(token.split('.').nth(1).unwrap())).unwrap();
        let exp = claims["exp"].as_u64().unwrap();
        let renewed_at = 1_800_000_000 + 12 * 3600;
        assert!(
            exp > renewed_at + 12 * 3600,
            "still valid when served for the last time"
        );
        assert!(exp - renewed_at <= 86_400);
    }

    #[test]
    fn vapid_key_is_private_and_stable() {
        let home = TempDir::new("vapid-private");
        let first = Vapid::load_or_create(&home.0).unwrap().public_key_b64url();
        let file = home.0.join("companion").join("vapid.key");
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(&home.0.join("companion")), 0o700);

        // Rouvrir garde la clé (les abonnements en dépendent) et resserre des droits élargis.
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let second = Vapid::load_or_create(&home.0).unwrap().public_key_b64url();
        assert_eq!(first, second);
        assert_eq!(mode(&file), 0o600);

        // Un fichier abîmé est une erreur, pas une nouvelle clé qui invaliderait tous les appairages.
        std::fs::write(&file, b"short").unwrap();
        let error = Vapid::load_or_create(&home.0).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    /// Un reste de création interrompue ne bloque rien, et deux appels simultanés finissent avec la même clé, sans
    /// jamais laisser de fichier voisin.
    #[test]
    fn vapid_creation_survives_a_stale_temporary_and_concurrent_calls() {
        let home = TempDir::new("vapid-atomic");
        let folder = home.0.join("companion");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("vapid.key.tmp"), b"").unwrap();

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let dir = home.0.clone();
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    Vapid::load_or_create(&dir).unwrap().public_key_b64url()
                })
            })
            .collect();
        let keys: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        assert!(keys.windows(2).all(|pair| pair[0] == pair[1]), "{keys:?}");
        assert_eq!(
            std::fs::metadata(folder.join("vapid.key")).unwrap().len(),
            32
        );
        let leftovers: Vec<String> = std::fs::read_dir(&folder)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name != "vapid.key" && name != "vapid.key.tmp")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn production_encryption_caps_the_plaintext_at_the_rfc_limit() {
        let ua_private = p256::SecretKey::random(&mut OsRng);
        let public = {
            use p256::elliptic_curve::sec1::ToEncodedPoint;
            ua_private.public_key().to_encoded_point(false)
        };
        let ua = UaKeys::from_subscription(&subscription(public.as_bytes(), &[3u8; 16])).unwrap();

        let accepted = encrypt(&vec![b'a'; 3993], &ua).unwrap();
        assert_eq!(accepted.len(), 4096, "a full payload is exactly 4096 bytes");
        assert_eq!(
            encrypt(&vec![b'a'; 3994], &ua),
            Err(PushError::PayloadTooLarge)
        );
    }

    #[test]
    fn headers_carry_the_delivery_policy() {
        let headers = headers("claude-1");
        let get = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(get("TTL"), Some("3600"));
        assert_eq!(get("Urgency"), Some("high"));
        assert_eq!(get("Topic"), Some("claude-1"));
        assert_eq!(get("Content-Encoding"), Some("aes128gcm"));
        assert_eq!(get("Content-Type"), Some("application/octet-stream"));
    }
}
