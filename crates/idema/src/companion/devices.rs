//! Les appareils appairés : `<dossier d'IdemA>/companion/devices.json` (dossier 0700, fichier 0600). On n'y garde que
//! l'empreinte SHA-256 du jeton, jamais le jeton. Un seul écrivain : le fil du service.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rand::RngCore;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};

/// La dernière activité d'un appareil n'est réécrite qu'une fois par cet intervalle.
const TOUCH_INTERVAL: Duration = Duration::from_secs(60);

/// Un abonnement push (Push API), tel que le navigateur le donne ; base64url.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscription {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub token_hash: String,
    pub paired_at: SystemTime,
    pub last_seen_at: SystemTime,
    pub subscription: Option<Subscription>,
}

/// La forme du fichier : les instants en secondes depuis l'époque Unix.
#[derive(Serialize, Deserialize)]
struct FileContent {
    devices: Vec<Record>,
}

#[derive(Serialize, Deserialize)]
struct Record {
    id: String,
    name: String,
    token_hash: String,
    paired_at: u64,
    last_seen_at: u64,
    subscription: Option<Subscription>,
}

fn to_seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn from_seconds(seconds: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds)
}

/// L'instant au grain de la seconde, celui que le fichier sait garder.
fn truncated(time: SystemTime) -> SystemTime {
    from_seconds(to_seconds(time))
}

impl From<&Device> for Record {
    fn from(device: &Device) -> Self {
        Self {
            id: device.id.clone(),
            name: device.name.clone(),
            token_hash: device.token_hash.clone(),
            paired_at: to_seconds(device.paired_at),
            last_seen_at: to_seconds(device.last_seen_at),
            subscription: device.subscription.clone(),
        }
    }
}

impl From<Record> for Device {
    fn from(record: Record) -> Self {
        Self {
            id: record.id,
            name: record.name,
            token_hash: record.token_hash,
            paired_at: from_seconds(record.paired_at),
            last_seen_at: from_seconds(record.last_seen_at),
            subscription: record.subscription,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DeviceStore {
    path: PathBuf,
    devices: Vec<Device>,
}

impl DeviceStore {
    /// Ouvre (et crée au besoin) `<dir>/companion/devices.json`. Un fichier illisible est une erreur : on ne repart
    /// pas d'une liste vide en silence, ce qui défairait tous les appairages.
    pub fn open(dir: &Path) -> io::Result<Self> {
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
        let path = folder.join("devices.json");
        let devices = match fs::read(&path) {
            Ok(bytes) => {
                // Un fichier laissé avec des droits plus larges (restauré, copié) est resserré avant tout usage.
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
                serde_json::from_slice::<FileContent>(&bytes)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
                    .devices
                    .into_iter()
                    .map(Device::from)
                    .collect()
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        Ok(Self { path, devices })
    }

    /// Ajoute un appareil et rend son identifiant.
    pub fn add(&mut self, name: &str, token_hash: &str, now: SystemTime) -> io::Result<String> {
        let mut raw = [0u8; 8];
        OsRng.fill_bytes(&mut raw);
        let id: String = raw.iter().map(|byte| format!("{byte:02x}")).collect();
        let now = truncated(now);
        self.mutate(|devices| {
            devices.push(Device {
                id: id.clone(),
                name: name.to_owned(),
                token_hash: token_hash.to_owned(),
                paired_at: now,
                last_seen_at: now,
                subscription: None,
            });
            Some(())
        })?;
        Ok(id)
    }

    pub fn by_token_hash(&self, hash: &str) -> Option<&Device> {
        self.devices.iter().find(|device| device.token_hash == hash)
    }

    /// Faut-il écrire la dernière activité de l'appareil ? Au plus une fois par minute.
    pub fn touch_due(&self, id: &str, now: SystemTime) -> bool {
        self.devices
            .iter()
            .find(|device| device.id == id)
            .is_some_and(|device| match now.duration_since(device.last_seen_at) {
                Ok(elapsed) => elapsed >= TOUCH_INTERVAL,
                // L'horloge est revenue en arrière : mieux vaut réécrire que rester figé sur un instant futur.
                Err(_) => true,
            })
    }

    /// Note la dernière activité ; le fichier n'est réécrit qu'au plus une fois par minute.
    pub fn touch(&mut self, id: &str, now: SystemTime) -> io::Result<()> {
        if !self.touch_due(id, now) {
            return Ok(());
        }
        let now = truncated(now);
        self.mutate(|devices| {
            devices
                .iter_mut()
                .find(|device| device.id == id)
                .map(|device| device.last_seen_at = now)
        })
        .map(|_| ())
    }

    /// Oublie l'appareil et son abonnement. Rend faux s'il n'existait pas.
    pub fn forget(&mut self, id: &str) -> io::Result<bool> {
        Ok(self
            .mutate(|devices| {
                let before = devices.len();
                devices.retain(|device| device.id != id);
                (devices.len() < before).then_some(())
            })?
            .is_some())
    }

    /// Pose l'abonnement de l'appareil. Un `endpoint` n'appartient qu'à un appareil : il est retiré des autres.
    pub fn set_subscription(&mut self, id: &str, sub: Subscription) -> io::Result<()> {
        let written = self.mutate(|devices| {
            devices.iter().position(|device| device.id == id)?;
            for device in devices.iter_mut() {
                if device
                    .subscription
                    .as_ref()
                    .is_some_and(|held| held.endpoint == sub.endpoint)
                {
                    device.subscription = None;
                }
            }
            devices
                .iter_mut()
                .find(|device| device.id == id)
                .map(|device| device.subscription = Some(sub))
        })?;
        written.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown device"))
    }

    /// Retire l'abonnement qui porte cet `endpoint`, chez qui qu'il soit. Rend faux s'il n'y en avait pas.
    pub fn remove_endpoint(&mut self, endpoint: &str) -> io::Result<bool> {
        Ok(self
            .mutate(|devices| {
                let holder = devices.iter_mut().find(|device| {
                    device
                        .subscription
                        .as_ref()
                        .is_some_and(|held| held.endpoint == endpoint)
                })?;
                holder.subscription = None;
                Some(())
            })?
            .is_some())
    }

    pub fn devices(&self) -> &[Device] {
        &self.devices
    }

    /// Applique `change` à la liste, puis l'écrit. `change` rend `None` quand il n'y avait rien à faire : rien n'est
    /// écrit. Si l'écriture échoue, la liste revient à ce qu'elle était : la mémoire ne devance jamais le disque.
    fn mutate<R>(
        &mut self,
        change: impl FnOnce(&mut Vec<Device>) -> Option<R>,
    ) -> io::Result<Option<R>> {
        let backup = self.devices.clone();
        let Some(result) = change(&mut self.devices) else {
            self.devices = backup;
            return Ok(None);
        };
        match self.persist() {
            Ok(()) => Ok(Some(result)),
            Err(error) => {
                self.devices = backup;
                Err(error)
            }
        }
    }

    /// Écrit le fichier d'un coup : un fichier voisin en 0600, puis un renommage.
    fn persist(&self) -> io::Result<()> {
        let content = FileContent {
            devices: self.devices.iter().map(Record::from).collect(),
        };
        let bytes = serde_json::to_vec_pretty(&content).map_err(io::Error::other)?;
        let temporary = self.path.with_extension("json.tmp");
        let written = (|| {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)
        })();
        if written.is_err() {
            // Le fichier voisin ne doit pas rester : l'échec d'origine est celui qu'on rend.
            let _ = fs::remove_file(&temporary);
        }
        written
    }
}

/// Le nom montré pour un appareil, tiré de son agent utilisateur.
pub fn device_name(user_agent: Option<&str>) -> &'static str {
    match user_agent {
        Some(agent) if agent.contains("iPhone") => "iPhone",
        Some(agent) if agent.contains("iPad") => "iPad",
        _ => "Appareil",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, UNIX_EPOCH};

    use crate::companion::test_support::TempDir;

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_800_000_000 + seconds)
    }

    fn subscription(endpoint: &str) -> Subscription {
        Subscription {
            endpoint: endpoint.into(),
            p256dh: "key".into(),
            auth: "auth".into(),
        }
    }

    #[test]
    fn devices_file_is_private() {
        let home = TempDir::new("private");
        let mut store = DeviceStore::open(&home.0).unwrap();
        store.add("iPhone", "hash-a", at(0)).unwrap();
        let folder = home.0.join("companion");
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&folder), 0o700);
        assert_eq!(mode(&folder.join("devices.json")), 0o600);
        // Un dossier et un fichier déjà là avec des droits trop larges sont resserrés à l'ouverture.
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(
            folder.join("devices.json"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        DeviceStore::open(&home.0).unwrap();
        assert_eq!(mode(&folder), 0o700);
        assert_eq!(mode(&folder.join("devices.json")), 0o600);
    }

    #[test]
    fn the_file_never_holds_a_token_or_a_stray_temp_file() {
        let home = TempDir::new("content");
        let mut store = DeviceStore::open(&home.0).unwrap();
        store.add("iPhone", "hash-a", at(0)).unwrap();
        let folder = home.0.join("companion");
        let names: Vec<_> = fs::read_dir(&folder)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["devices.json"]);
        let text = fs::read_to_string(folder.join("devices.json")).unwrap();
        assert!(text.contains("hash-a"));
    }

    #[test]
    fn touch_writes_at_most_once_a_minute() {
        let home = TempDir::new("touch");
        let mut store = DeviceStore::open(&home.0).unwrap();
        let id = store.add("iPhone", "hash-a", at(0)).unwrap();
        let file = home.0.join("companion").join("devices.json");
        // Le fichier prend une date ancienne : seule une réécriture la fait bouger.
        let old = UNIX_EPOCH + Duration::from_secs(1_000_000);
        fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let modified = || fs::metadata(&file).unwrap().modified().unwrap();

        assert!(!store.touch_due(&id, at(30)));
        store.touch(&id, at(30)).unwrap();
        assert_eq!(modified(), old, "no write 30 s after the last activity");
        assert_eq!(store.devices()[0].last_seen_at, at(0));

        assert!(store.touch_due(&id, at(60)));
        store.touch(&id, at(61)).unwrap();
        assert_ne!(modified(), old, "written a minute later");
        assert_eq!(store.devices()[0].last_seen_at, at(61));
        let reopened = DeviceStore::open(&home.0).unwrap();
        assert_eq!(reopened.devices()[0].last_seen_at, at(61));
    }

    #[test]
    fn an_endpoint_is_kept_once_across_devices() {
        let home = TempDir::new("endpoint");
        let mut store = DeviceStore::open(&home.0).unwrap();
        let phone = store.add("iPhone", "hash-a", at(0)).unwrap();
        let tablet = store.add("iPad", "hash-b", at(1)).unwrap();
        store
            .set_subscription(&phone, subscription("https://push.example/1"))
            .unwrap();
        store
            .set_subscription(&tablet, subscription("https://push.example/1"))
            .unwrap();
        let holders: Vec<_> = store
            .devices()
            .iter()
            .filter(|device| device.subscription.is_some())
            .map(|device| device.id.clone())
            .collect();
        assert_eq!(holders, std::slice::from_ref(&tablet));
        assert!(store.remove_endpoint("https://push.example/1").unwrap());
        assert!(!store.remove_endpoint("https://push.example/1").unwrap());
        assert!(
            store
                .devices()
                .iter()
                .all(|device| device.subscription.is_none())
        );
        let missing = store.set_subscription("absent", subscription("https://push.example/2"));
        assert_eq!(missing.unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn forgetting_one_device_keeps_the_other() {
        let home = TempDir::new("forget");
        let mut store = DeviceStore::open(&home.0).unwrap();
        let phone = store.add("iPhone", "hash-a", at(0)).unwrap();
        let tablet = store.add("iPad", "hash-b", at(1)).unwrap();
        store
            .set_subscription(&phone, subscription("https://push.example/phone"))
            .unwrap();
        store
            .set_subscription(&tablet, subscription("https://push.example/tablet"))
            .unwrap();
        assert!(store.forget(&phone).unwrap());
        assert!(!store.forget(&phone).unwrap());
        assert!(store.by_token_hash("hash-a").is_none());
        let kept = store.by_token_hash("hash-b").expect("the iPad is kept");
        assert_eq!(kept.id, tablet);
        assert_eq!(
            kept.subscription.as_ref().map(|sub| sub.endpoint.as_str()),
            Some("https://push.example/tablet")
        );
        let reopened = DeviceStore::open(&home.0).unwrap();
        assert!(reopened.by_token_hash("hash-a").is_none());
        assert!(reopened.by_token_hash("hash-b").is_some());
    }

    #[test]
    fn devices_survive_a_reload() {
        let home = TempDir::new("reload");
        let id = {
            let mut store = DeviceStore::open(&home.0).unwrap();
            let id = store.add("iPhone", "hash-a", at(5)).unwrap();
            store
                .set_subscription(&id, subscription("https://push.example/1"))
                .unwrap();
            id
        };
        let store = DeviceStore::open(&home.0).unwrap();
        let device = store
            .by_token_hash("hash-a")
            .expect("the token is still known");
        assert_eq!(device.id, id);
        assert_eq!(device.name, "iPhone");
        assert_eq!(device.paired_at, at(5));
        assert_eq!(device.last_seen_at, at(5));
        assert!(device.subscription.is_some());
    }

    #[test]
    fn identifiers_are_unique_and_not_the_hash() {
        let home = TempDir::new("ids");
        let mut store = DeviceStore::open(&home.0).unwrap();
        let first = store.add("iPhone", "hash-a", at(0)).unwrap();
        let second = store.add("iPhone", "hash-b", at(0)).unwrap();
        assert_ne!(first, second);
        assert!(!first.is_empty() && !"hash-a".contains(&first));
    }

    #[test]
    fn a_corrupt_file_is_an_error_not_an_empty_list() {
        let home = TempDir::new("corrupt");
        let mut store = DeviceStore::open(&home.0).unwrap();
        store.add("iPhone", "hash-a", at(0)).unwrap();
        fs::write(home.0.join("companion").join("devices.json"), b"{ not json").unwrap();
        let error = DeviceStore::open(&home.0).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_failed_write_leaves_the_list_unchanged() {
        let home = TempDir::new("rollback");
        let mut store = DeviceStore::open(&home.0).unwrap();
        let id = store.add("iPhone", "hash-a", at(0)).unwrap();
        let folder = home.0.join("companion");
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o500)).unwrap();
        let failed = store.add("iPad", "hash-b", at(1));
        let forgot = store.forget(&id);
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o700)).unwrap();
        // Lancés en root (certaines VM), le dossier reste inscriptible : rien à constater alors.
        if failed.is_err() {
            assert!(forgot.is_err());
            assert_eq!(
                store.devices().len(),
                1,
                "the device list is not ahead of the disk"
            );
            assert!(store.by_token_hash("hash-b").is_none());
            assert!(store.by_token_hash("hash-a").is_some());
        }
    }

    #[test]
    fn device_names_come_from_the_user_agent() {
        assert_eq!(
            device_name(Some(
                "Mozilla/5.0 (iPhone; CPU iPhone OS 19_0 like Mac OS X) AppleWebKit/605.1.15"
            )),
            "iPhone"
        );
        assert_eq!(
            device_name(Some("Mozilla/5.0 (iPad; CPU OS 19_0 like Mac OS X)")),
            "iPad"
        );
        assert_eq!(
            device_name(Some("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)")),
            "Appareil"
        );
        assert_eq!(device_name(None), "Appareil");
    }
}
