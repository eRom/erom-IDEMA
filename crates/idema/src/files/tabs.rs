//! Les onglets du volet Fichiers et les règles de sauvegarde d'un onglet (spec P2 §4.5 et §4.6). Aucune dépendance à
//! GPUI : la barre d'onglets (qui est ouvert, lequel est en aperçu, lequel est actif) et l'arbitrage entre le texte de
//! l'éditeur, la dernière écriture d'IdemA et le disque se testent sans fenêtre.

use std::borrow::Borrow;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// La clé d'un onglet : le chemin canonique du fichier (arbitrage 5), qui est aussi ce que la surveillance du disque
/// rapporte. Elle s'emprunte comme un `Path`, pour qu'un lot de la surveillance se compare sans copier ses chemins.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TabKey(pub PathBuf);

impl Borrow<Path> for TabKey {
    fn borrow(&self) -> &Path {
        &self.0
    }
}

#[derive(Debug)]
struct Tab {
    path: PathBuf,
    /// Un onglet d'aperçu (titre en italique) est remplacé par l'ouverture suivante ; une frappe ou un double-clic
    /// le rend permanent.
    preview: bool,
}

/// La barre d'onglets : l'ordre des fichiers ouverts et l'onglet actif.
#[derive(Debug, Default)]
pub struct TabStrip {
    tabs: Vec<Tab>,
    active: Option<usize>,
}

impl TabStrip {
    /// Ouvre `path` et le rend actif. Un chemin déjà ouvert redevient actif (`pin` le rend permanent, jamais
    /// l'inverse). Sinon le fichier prend la place de l'onglet d'aperçu s'il y en a un, ou s'ajoute à la fin. Rend
    /// l'indice de l'onglet.
    pub fn open(&mut self, path: PathBuf, pin: bool) -> usize {
        if let Some(index) = self.index_of(&path) {
            if pin {
                self.tabs[index].preview = false;
            }
            self.active = Some(index);
            return index;
        }
        let tab = Tab {
            path,
            preview: !pin,
        };
        let index = match self.tabs.iter().position(|tab| tab.preview) {
            Some(index) => {
                self.tabs[index] = tab;
                index
            }
            None => {
                self.tabs.push(tab);
                self.tabs.len() - 1
            }
        };
        self.active = Some(index);
        index
    }

    /// Rend l'onglet permanent (frappe, double-clic).
    pub fn pin(&mut self, index: usize) {
        if let Some(tab) = self.tabs.get_mut(index) {
            tab.preview = false;
        }
    }

    /// Rend actif l'onglet `index`, s'il existe.
    pub fn activate(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.active = Some(index);
        }
    }

    /// Ferme l'onglet `index` et rend l'onglet actif qui en résulte : si c'était l'actif, son voisin de droite (ou de
    /// gauche s'il était le dernier) ; sinon l'actif d'avant, dont l'indice peut avoir reculé d'un cran.
    pub fn close(&mut self, index: usize) -> Option<usize> {
        if index >= self.tabs.len() {
            return self.active;
        }
        self.tabs.remove(index);
        self.active = match self.active {
            _ if self.tabs.is_empty() => None,
            Some(active) if active == index => Some(index.min(self.tabs.len() - 1)),
            Some(active) if active > index => Some(active - 1),
            other => other,
        };
        self.active
    }

    pub fn active(&self) -> Option<usize> {
        self.active
    }

    pub fn index_of(&self, path: &Path) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.path == path)
    }

    pub fn paths(&self) -> Vec<&Path> {
        self.tabs.iter().map(|tab| tab.path.as_path()).collect()
    }

    pub fn is_preview(&self, index: usize) -> bool {
        self.tabs.get(index).is_some_and(|tab| tab.preview)
    }

    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    /// Les chemins des onglets ouverts sur `prefix` ou dessous (par composants : `/p/src2` n'est pas sous `/p/src`),
    /// dans l'ordre des onglets.
    pub fn paths_under(&self, prefix: &Path) -> Vec<PathBuf> {
        self.tabs
            .iter()
            .filter(|tab| tab.path.starts_with(prefix))
            .map(|tab| tab.path.clone())
            .collect()
    }

    /// Ferme les onglets ouverts sur `prefix` ou dessous et rend leurs chemins. Sert avant un renommage ou un déplacement
    /// qui arrive sur `prefix` : la cible n'existait pas (le renommage refuse d'écraser), donc seuls des onglets
    /// « supprimé » peuvent y rester, et ils ne doivent pas doubler l'onglet qui arrive.
    pub fn close_under(&mut self, prefix: &Path) -> Vec<PathBuf> {
        let dropped = self.paths_under(prefix);
        // De la fin au début : fermer un onglet ne décale que ceux qui le suivent.
        for index in (0..self.tabs.len()).rev() {
            if self.tabs[index].path.starts_with(prefix) {
                self.close(index);
            }
        }
        dropped
    }

    /// `from` a été renommé ou déplacé en `to` : chaque onglet sur `from` ou dessous prend `to` suivi du reste de son
    /// chemin. L'ordre, l'aperçu et l'onglet actif ne bougent pas. Rend les couples (ancien, nouveau) dans l'ordre des
    /// onglets, de quoi suivre aussi l'éditeur et la surveillance.
    pub fn rename_under(&mut self, from: &Path, to: &Path) -> Vec<(PathBuf, PathBuf)> {
        let mut moved = Vec::new();
        for tab in &mut self.tabs {
            let Ok(rest) = tab.path.strip_prefix(from) else {
                continue;
            };
            // `join` d'un reste vide ajouterait une barre finale : le chemin du dossier lui-même reste tel quel.
            let new_path = if rest.as_os_str().is_empty() {
                to.to_path_buf()
            } else {
                to.join(rest)
            };
            moved.push((std::mem::replace(&mut tab.path, new_path.clone()), new_path));
        }
        moved
    }
}

/// L'empreinte d'un fichier sur le disque : sa taille et sa date de modification. Elle est relevée à l'ouverture, à
/// chaque rechargement et à chaque écriture d'IdemA ; un fichier qui en a une autre au moment de sauvegarder a été
/// écrit par quelqu'un d'autre depuis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint {
    pub len: u64,
    pub modified: Option<SystemTime>,
}

/// L'empreinte de `path` : `Ok(None)` quand le fichier n'existe pas (ou plus), `Err` pour toute autre panne de lecture
/// des métadonnées.
pub fn fingerprint(path: &Path) -> io::Result<Option<Fingerprint>> {
    match std::fs::metadata(path) {
        Ok(meta) => Ok(Some(Fingerprint {
            len: meta.len(),
            modified: meta.modified().ok(),
        })),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Ce que dit la comparaison de l'empreinte du disque avant d'écrire (`SaveState::check_write`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveCheck {
    /// Le disque est tel qu'IdemA l'a laissé : on peut écrire.
    Write,
    /// Le disque a changé (ou le fichier a disparu) depuis : on n'écrit pas, le disque gagne (P2-7) et l'onglet se
    /// recharge.
    DiskChanged,
}

/// Ce que le disque dit à un onglet quand le fichier change (`SaveState::on_disk_change`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskVerdict {
    /// Le disque dit ce que l'onglet sait déjà (notre propre écriture qui revient, un simple `touch`) : rien à faire,
    /// le curseur ne bouge pas (risque P2-R4).
    Ignore,
    /// Le disque a un autre contenu : l'onglet le reprend. `pending_edit` dit que des frappes attendaient leur
    /// sauvegarde et sont perdues, le disque gagne (P2-7).
    Reload { pending_edit: bool },
    /// Le fichier n'existe plus.
    Deleted,
}

/// L'état de sauvegarde d'un onglet, en règles pures. `saved` est le texte que le disque a, à notre connaissance (lu
/// à l'ouverture ou au dernier rechargement, ou écrit par IdemA) ; `pending` est la génération de la dernière frappe
/// non sauvegardée.
#[derive(Debug)]
pub struct SaveState {
    saved: String,
    pending: Option<u64>,
    deleted: bool,
    error: Option<String>,
    /// Le disque porte maintenant un fichier que l'éditeur ne sait pas lire (binaire, non UTF-8, trop gros, refusé) :
    /// l'onglet garde son texte, ne sauvegarde plus rien par-dessus et ne relit plus à chaque tentative.
    unreadable: Option<String>,
    /// L'empreinte que le fichier avait quand `saved` en a été lu ou écrit ; `None` quand il n'existe pas.
    fingerprint: Option<Fingerprint>,
}

impl SaveState {
    pub fn new(saved: String) -> Self {
        Self {
            saved,
            pending: None,
            deleted: false,
            error: None,
            unreadable: None,
            fingerprint: None,
        }
    }

    /// L'empreinte que le disque avait au moment de la dernière lecture ou écriture d'IdemA.
    pub fn set_fingerprint(&mut self, fingerprint: Option<Fingerprint>) {
        self.fingerprint = fingerprint;
    }

    pub fn fingerprint(&self) -> Option<Fingerprint> {
        self.fingerprint
    }

    /// Avant d'écrire : `disk` est l'empreinte actuelle du fichier. Une autre empreinte que celle qu'on connaît (un
    /// agent a écrit dans les 100 ms que met la surveillance à le dire, ou le fichier n'est pas surveillé) ou un
    /// fichier disparu interdisent d'écrire par-dessus. Un fichier connu pour supprimé peut être recréé tant qu'il
    /// n'a pas reparu.
    pub fn check_write(&self, disk: Option<Fingerprint>) -> SaveCheck {
        if disk == self.fingerprint {
            SaveCheck::Write
        } else {
            SaveCheck::DiskChanged
        }
    }

    /// Une frappe, numérotée par `generation` : la sauvegarde d'une seconde n'est due que pour la dernière.
    pub fn on_edit(&mut self, generation: u64) {
        // Un disque illisible n'est jamais réécrit : la frappe reste dans l'éditeur, sans sauvegarde.
        if self.unreadable.is_none() {
            self.pending = Some(generation);
        }
    }

    /// Le minuteur de la frappe `generation` a sonné : est-ce encore la dernière frappe, et le disque ne l'a-t-il pas
    /// annulée entre-temps ?
    pub fn due(&self, generation: u64) -> bool {
        self.pending == Some(generation)
    }

    /// Y a-t-il un texte à écrire : des frappes en attente, ou une écriture refusée à rejouer ?
    pub fn needs_save(&self) -> bool {
        self.unreadable.is_none() && (self.pending.is_some() || self.error.is_some())
    }

    /// Fermer l'onglet perdrait-il du texte ? Oui tant qu'il reste quelque chose à écrire après l'essai de sauvegarde
    /// de la fermeture : l'onglet reste alors ouvert, avec sa marque (ruling 21). Non quand `buffer`, le texte de
    /// l'éditeur, est celui que le disque a déjà (une annulation `⌘Z` est revenue à l'original) : rien ne se perd
    /// (ruling 22).
    pub fn blocks_close(&self, buffer: &str) -> bool {
        self.needs_save() && !self.holds(buffer)
    }

    /// `buffer`, le texte de l'éditeur, est-il celui que le disque porte (le dernier lu ou écrit par IdemA) ?
    pub fn holds(&self, buffer: &str) -> bool {
        buffer == self.saved
    }

    /// Le disque porte un fichier que l'éditeur ne sait pas lire. Rend vrai la première fois de la série (le journal
    /// ne le dit qu'une fois). Les frappes en attente tombent : le disque gagne (P2-7).
    pub fn on_unreadable(&mut self, error: String) -> bool {
        let first = self.unreadable.is_none();
        self.pending = None;
        self.error = None;
        self.unreadable = Some(error);
        first
    }
    /// `text` vient d'être écrit sur le disque.
    pub fn on_saved(&mut self, text: String) {
        self.saved = text;
        self.pending = None;
        self.deleted = false;
        self.error = None;
    }

    /// L'écriture a été refusée : le texte reste dans l'onglet, la prochaine frappe réessaie. Rend vrai pour la
    /// première panne de la série (la bulle ne s'affiche qu'une fois).
    pub fn on_save_failed(&mut self, error: String) -> bool {
        let first = self.error.is_none();
        self.pending = None;
        self.error = Some(error);
        first
    }

    /// Le disque a changé : `disk` est son contenu (`None` quand le fichier n'existe plus), `buffer` le texte de
    /// l'éditeur. Le disque gagne toujours, la sauvegarde en attente est annulée (P2-7) ; il n'y a rien à reprendre
    /// quand il égale le tampon ou ce qu'IdemA y a écrit en dernier.
    pub fn on_disk_change(&mut self, disk: Option<&str>, buffer: &str) -> DiskVerdict {
        // Le disque se lit de nouveau (ou a disparu) : la panne de lecture est finie.
        self.unreadable = None;
        let Some(disk) = disk else {
            // Le fichier a disparu : ni les frappes en attente ni une écriture refusée à rejouer ne le recréeront, il
            // faut une nouvelle frappe.
            self.pending = None;
            self.error = None;
            self.deleted = true;
            self.fingerprint = None;
            return DiskVerdict::Deleted;
        };
        self.deleted = false;
        if disk == buffer {
            // Le disque porte déjà le tampon (notre écriture, ou un agent qui a écrit la même chose) : rien à garder.
            self.saved = disk.to_owned();
            self.pending = None;
            self.error = None;
            return DiskVerdict::Ignore;
        }
        if disk == self.saved {
            // Le disque est celui que nous connaissons, l'onglet a de l'avance : les frappes en attente restent.
            return DiskVerdict::Ignore;
        }
        let pending_edit = self.pending.take().is_some();
        self.saved = disk.to_owned();
        self.error = None;
        DiskVerdict::Reload { pending_edit }
    }

    pub fn is_deleted(&self) -> bool {
        self.deleted
    }

    /// Le disque porte un fichier que l'éditeur ne sait plus lire : l'onglet ne sauvegarde plus rien.
    pub fn is_unreadable(&self) -> bool {
        self.unreadable.is_some()
    }

    /// La marque de l'onglet, en texte secondaire : « supprimé », « illisible » (le disque n'est plus du texte), puis
    /// « non sauvegardé ». Cette dernière ne marque
    /// qu'une écriture refusée (spec §5), pas la seconde d'attente de la sauvegarde automatique.
    pub fn mark(&self) -> Option<&'static str> {
        if self.is_deleted() {
            Some("supprimé")
        } else if self.unreadable.is_some() {
            Some("illisible")
        } else if self.error.is_some() {
            Some("non sauvegardé")
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(name: &str) -> PathBuf {
        PathBuf::from(format!("/project/{name}"))
    }

    #[test]
    fn a_preview_tab_is_replaced_by_the_next_single_click() {
        let mut strip = TabStrip::default();
        assert_eq!(strip.open(path("a"), false), 0);
        assert_eq!(strip.open(path("b"), false), 0);
        assert_eq!(strip.paths(), [path("b").as_path()]);
        assert!(strip.is_preview(0));
        assert_eq!(strip.active(), Some(0));
    }

    #[test]
    fn pinning_right_after_a_preview_open_is_the_same_as_a_pinned_open() {
        // `FilesPanel::open_path` ouvre en aperçu puis épingle : même barre que `open(.., true)`.
        let mut opened_then_pinned = TabStrip::default();
        opened_then_pinned.open(path("a"), true);
        opened_then_pinned.open(path("b"), false);
        let index = opened_then_pinned.index_of(&path("b")).unwrap();
        opened_then_pinned.pin(index);
        let mut pinned_open = TabStrip::default();
        pinned_open.open(path("a"), true);
        pinned_open.open(path("b"), true);
        assert_eq!(opened_then_pinned.paths(), pinned_open.paths());
        assert!(!opened_then_pinned.is_preview(index));
        // Sans épingle, `b` reste un aperçu que le fichier suivant remplace.
        let mut preview = TabStrip::default();
        preview.open(path("a"), true);
        preview.open(path("b"), false);
        preview.open(path("c"), false);
        assert_eq!(preview.paths(), [path("a").as_path(), path("c").as_path()]);
    }

    #[test]
    fn a_pinned_tab_stays_when_another_file_opens() {
        let mut strip = TabStrip::default();
        strip.open(path("a"), true);
        assert_eq!(strip.open(path("b"), false), 1);
        assert_eq!(strip.paths(), [path("a").as_path(), path("b").as_path()]);
        assert!(!strip.is_preview(0));
        assert!(strip.is_preview(1));
        // Le suivant prend la place de l'aperçu, pas celle de l'onglet permanent.
        assert_eq!(strip.open(path("c"), false), 1);
        assert_eq!(strip.paths(), [path("a").as_path(), path("c").as_path()]);
        // Une frappe (ou un double-clic) le rend permanent : le suivant s'ajoute.
        strip.pin(1);
        assert_eq!(strip.open(path("d"), false), 2);
        assert_eq!(strip.paths().len(), 3);
    }

    #[test]
    fn opening_an_open_path_activates_it() {
        let mut strip = TabStrip::default();
        strip.open(path("a"), true);
        strip.open(path("b"), true);
        assert_eq!(strip.active(), Some(1));
        assert_eq!(strip.open(path("a"), false), 0);
        assert_eq!(strip.active(), Some(0));
        assert_eq!(strip.paths().len(), 2);
        // Rouvrir en aperçu ne défait pas l'épinglage ; un double-clic sur un aperçu l'épingle.
        assert!(!strip.is_preview(0));
        strip.open(path("c"), false);
        assert!(strip.is_preview(2));
        strip.open(path("c"), true);
        assert!(!strip.is_preview(2));
    }

    #[test]
    fn closing_the_active_tab_activates_its_neighbour() {
        let mut strip = TabStrip::default();
        for name in ["a", "b", "c"] {
            strip.open(path(name), true);
        }
        // Le dernier : son voisin de gauche.
        assert_eq!(strip.close(2), Some(1));
        strip.open(path("c"), true);
        strip.activate(0);
        // Le premier : celui qui prend sa place.
        assert_eq!(strip.close(0), Some(0));
        assert_eq!(strip.paths(), [path("b").as_path(), path("c").as_path()]);
        // Fermer un onglet avant l'actif décale l'indice de l'actif sans changer l'onglet.
        strip.activate(1);
        assert_eq!(strip.close(0), Some(0));
        assert_eq!(strip.paths(), [path("c").as_path()]);
        // Fermer un onglet après l'actif ne le touche pas.
        strip.open(path("d"), true);
        strip.activate(0);
        assert_eq!(strip.close(1), Some(0));
        assert_eq!(strip.close(0), None);
        assert!(strip.is_empty());
    }

    #[test]
    fn a_renamed_file_keeps_its_tab_preview_and_activity() {
        let mut strip = TabStrip::default();
        strip.open(path("a"), true);
        strip.open(path("c"), true);
        strip.open(path("b"), false);
        strip.activate(1);
        let moved = strip.rename_under(&path("b"), &path("renamed"));
        assert_eq!(moved, [(path("b"), path("renamed"))]);
        // Même place, même aperçu, même onglet actif : seul le chemin change.
        assert_eq!(
            strip.paths(),
            [
                path("a").as_path(),
                path("c").as_path(),
                path("renamed").as_path()
            ]
        );
        assert!(strip.is_preview(2) && !strip.is_preview(0) && !strip.is_preview(1));
        assert_eq!(strip.active(), Some(1));
        assert_eq!(strip.index_of(&path("renamed")), Some(2));
        assert_eq!(strip.index_of(&path("b")), None);
        // Un chemin sans onglet ne change rien.
        assert!(strip.rename_under(&path("zzz"), &path("yyy")).is_empty());
    }

    #[test]
    fn moving_a_folder_moves_every_tab_below_it_and_only_those() {
        let mut strip = TabStrip::default();
        strip.open(PathBuf::from("/p/src/a.rs"), true);
        strip.open(PathBuf::from("/p/src/deep/b.rs"), true);
        strip.open(PathBuf::from("/p/src2/c.rs"), true);
        let src = Path::new("/p/src");
        assert_eq!(
            strip.paths_under(src),
            [
                PathBuf::from("/p/src/a.rs"),
                PathBuf::from("/p/src/deep/b.rs")
            ]
        );
        let moved = strip.rename_under(src, Path::new("/p/lib"));
        assert_eq!(
            moved,
            [
                (PathBuf::from("/p/src/a.rs"), PathBuf::from("/p/lib/a.rs")),
                (
                    PathBuf::from("/p/src/deep/b.rs"),
                    PathBuf::from("/p/lib/deep/b.rs")
                ),
            ]
        );
        assert_eq!(
            strip.paths(),
            [
                Path::new("/p/lib/a.rs"),
                Path::new("/p/lib/deep/b.rs"),
                Path::new("/p/src2/c.rs")
            ]
        );
        // Les chemins stockés n'ont pas de barre finale parasite quand le dossier lui-même est ouvert.
        let mut strip = TabStrip::default();
        strip.open(PathBuf::from("/p/a"), true);
        strip.rename_under(Path::new("/p/a"), Path::new("/p/b"));
        assert_eq!(strip.paths()[0].as_os_str(), "/p/b");
        assert_eq!(strip.index_of(Path::new("/p/b")), Some(0));
    }

    #[test]
    fn a_rename_onto_a_deleted_tab_leaves_one_tab_per_path() {
        // `spec.md` a disparu (l'agent l'a supprimé) mais son onglet reste, marqué « supprimé » ; l'utilisateur renomme
        // `spec-v2.md` en `spec.md`.
        let mut strip = TabStrip::default();
        strip.open(path("notes.md"), true);
        strip.open(path("spec.md"), true);
        strip.open(path("spec-v2.md"), true);
        strip.activate(2);
        let dropped = strip.close_under(&path("spec.md"));
        assert_eq!(dropped, [path("spec.md")]);
        strip.rename_under(&path("spec-v2.md"), &path("spec.md"));
        let paths = strip.paths();
        let mut unique = paths.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(paths.len(), unique.len(), "one tab per path");
        assert_eq!(
            paths,
            [path("notes.md").as_path(), path("spec.md").as_path()]
        );
        // L'onglet renommé reste l'actif, et l'indice est valide.
        let active = strip.active().expect("an active tab");
        assert_eq!(strip.paths()[active], path("spec.md").as_path());
        // Un dossier déposé sur le chemin d'un dossier dont des onglets sont « supprimés » les ferme tous, et eux seuls.
        let mut strip = TabStrip::default();
        strip.open(PathBuf::from("/p/docs/a.md"), true);
        strip.open(PathBuf::from("/p/docs2/b.md"), true);
        strip.open(PathBuf::from("/p/new/c.md"), true);
        let dropped = strip.close_under(Path::new("/p/docs"));
        assert_eq!(dropped, [PathBuf::from("/p/docs/a.md")]);
        assert_eq!(strip.paths().len(), 2);
        assert!(strip.active().is_some_and(|active| active < 2));
    }

    #[test]
    fn a_disk_change_cancels_the_pending_save() {
        let mut state = SaveState::new("before".into());
        state.on_edit(1);
        assert!(state.due(1));
        let verdict = state.on_disk_change(Some("agent"), "mine");
        assert_eq!(verdict, DiskVerdict::Reload { pending_edit: true });
        // Le minuteur d'une seconde sonne ensuite : il ne repart pas écraser l'agent.
        assert!(!state.due(1));
        assert!(!state.needs_save());
        assert_eq!(state.mark(), None);
    }

    #[test]
    fn our_own_write_does_not_reload() {
        let mut state = SaveState::new(String::new());
        state.on_edit(1);
        state.on_saved("x".into());
        assert_eq!(state.on_disk_change(Some("x"), "x"), DiskVerdict::Ignore);
        assert_eq!(state.mark(), None);
    }

    #[test]
    fn our_write_coming_back_after_more_typing_keeps_the_new_typing() {
        // L'écriture revient par la surveillance alors que l'utilisateur a déjà retapé : le disque est ce que nous
        // venons d'y mettre, pas un changement extérieur.
        let mut state = SaveState::new(String::new());
        state.on_saved("x".into());
        state.on_edit(2);
        assert_eq!(state.on_disk_change(Some("x"), "xy"), DiskVerdict::Ignore);
        assert!(state.due(2));
    }

    #[test]
    fn a_deleted_file_is_not_recreated_until_typed() {
        let mut state = SaveState::new("x".into());
        assert_eq!(state.on_disk_change(None, "x"), DiskVerdict::Deleted);
        assert!(state.is_deleted());
        assert_eq!(state.mark(), Some("supprimé"));
        assert!(!state.due(1));
        assert!(!state.needs_save());
        state.on_edit(2);
        assert!(state.due(2));
        // La sauvegarde recrée le fichier : la marque tombe.
        state.on_saved("xy".into());
        assert_eq!(state.mark(), None);
    }

    #[test]
    fn a_crlf_file_opened_edited_and_saved_stays_crlf_without_reloading() {
        use crate::files::document::{Opened, read_document, save_atomically};

        let dir = std::env::temp_dir().join(format!("idema-tabs-{}-crlf", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.yml");
        std::fs::write(&file, b"a:\r\n\tb: 1\r\n").unwrap();
        let Opened::Text(loaded) = read_document(&file).unwrap() else {
            panic!("a.yml should open as text");
        };

        // Le tampon de l'éditeur est en `\n` : une ligne tapée, puis la sauvegarde de l'onglet.
        let mut state = SaveState::new(loaded.text.clone());
        let edited = format!("{}\tc: 2\n", loaded.text);
        state.on_edit(1);
        assert!(state.due(1));
        save_atomically(&file, &edited, loaded.line_ending).unwrap();
        state.on_saved(edited.clone());
        // Chaque ligne finit en `\r\n`, les tabulations sont intactes.
        assert_eq!(std::fs::read(&file).unwrap(), b"a:\r\n\tb: 1\r\n\tc: 2\r\n");

        // L'écriture revient par la surveillance : le disque relu, ramené en `\n`, égale le tampon.
        let Opened::Text(back) = read_document(&file).unwrap() else {
            panic!("a.yml should still open as text");
        };
        assert_eq!(
            state.on_disk_change(Some(&back.text), &edited),
            DiskVerdict::Ignore
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn print(len: u64, seconds: u64) -> Option<Fingerprint> {
        Some(Fingerprint {
            len,
            modified: Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(seconds)),
        })
    }

    #[test]
    fn a_save_over_a_disk_that_changed_since_is_routed_to_a_reload_and_writes_nothing() {
        let mut state = SaveState::new("before".into());
        state.set_fingerprint(print(6, 10));
        state.on_edit(1);
        // Rien n'a bougé : on écrit.
        assert_eq!(state.check_write(print(6, 10)), SaveCheck::Write);
        // Un agent a écrit dans la fenêtre de la surveillance : même taille, autre date, ou autre taille.
        assert_eq!(state.check_write(print(6, 11)), SaveCheck::DiskChanged);
        assert_eq!(state.check_write(print(9, 10)), SaveCheck::DiskChanged);
        // Le fichier a disparu : on ne le recrée pas.
        assert_eq!(state.check_write(None), SaveCheck::DiskChanged);
        // Le rechargement qui suit : le disque gagne, la frappe en attente est perdue, rien n'est écrit.
        let verdict = state.on_disk_change(Some("agent"), "mine");
        assert_eq!(verdict, DiskVerdict::Reload { pending_edit: true });
        assert!(!state.due(1));
        assert!(!state.needs_save());
    }

    #[test]
    fn the_fingerprint_follows_every_read_and_write_of_the_file() {
        let dir = std::env::temp_dir().join(format!("idema-tabs-{}-print", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        assert_eq!(fingerprint(&file).unwrap(), None);
        std::fs::write(&file, "one").unwrap();
        let opened = fingerprint(&file).unwrap();
        assert!(opened.is_some());

        let mut state = SaveState::new("one".into());
        state.set_fingerprint(opened);
        state.on_edit(1);
        assert_eq!(
            state.check_write(fingerprint(&file).unwrap()),
            SaveCheck::Write
        );
        // Un agent réécrit le fichier (autre taille) avant la sauvegarde.
        std::fs::write(&file, "agent wrote more").unwrap();
        assert_eq!(
            state.check_write(fingerprint(&file).unwrap()),
            SaveCheck::DiskChanged
        );
        // Une fois le rechargement fait, l'empreinte reprise permet d'écrire de nouveau.
        state.on_disk_change(Some("agent wrote more"), "mine");
        state.set_fingerprint(fingerprint(&file).unwrap());
        state.on_edit(2);
        assert_eq!(
            state.check_write(fingerprint(&file).unwrap()),
            SaveCheck::Write
        );
        // Le fichier supprimé : l'onglet le sait, il peut le recréer à une nouvelle frappe, pas si un fichier y est reparu.
        std::fs::remove_file(&file).unwrap();
        assert_eq!(
            state.check_write(fingerprint(&file).unwrap()),
            SaveCheck::DiskChanged
        );
        state.on_disk_change(None, "mine");
        assert_eq!(
            state.check_write(fingerprint(&file).unwrap()),
            SaveCheck::Write
        );
        std::fs::write(&file, "back").unwrap();
        assert_eq!(
            state.check_write(fingerprint(&file).unwrap()),
            SaveCheck::DiskChanged
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_save_on_a_file_that_then_disappears_does_not_recreate_it() {
        let mut state = SaveState::new("x".into());
        state.on_edit(1);
        state.on_save_failed("Permission denied".into());
        assert!(state.needs_save());
        assert_eq!(state.on_disk_change(None, "xy"), DiskVerdict::Deleted);
        // Plus rien à rejouer : seule une frappe remet l'onglet en attente d'écriture.
        assert!(!state.needs_save());
        assert_eq!(state.mark(), Some("supprimé"));
        state.on_edit(2);
        assert!(state.needs_save());
    }

    #[test]
    fn only_a_refused_write_marks_the_tab_not_the_wait_for_the_autosave() {
        let mut state = SaveState::new("x".into());
        state.on_edit(1);
        assert!(state.needs_save());
        assert_eq!(state.mark(), None);
        state.on_save_failed("Permission denied".into());
        assert_eq!(state.mark(), Some("non sauvegardé"));
    }

    #[test]
    fn a_file_that_comes_back_is_reloaded() {
        let mut state = SaveState::new("x".into());
        state.on_disk_change(None, "x");
        let verdict = state.on_disk_change(Some("again"), "x");
        assert_eq!(
            verdict,
            DiskVerdict::Reload {
                pending_edit: false
            }
        );
        assert!(!state.is_deleted());
    }

    #[test]
    fn a_refused_write_marks_the_tab_and_the_next_keystroke_retries() {
        let mut state = SaveState::new("x".into());
        state.on_edit(1);
        assert!(state.on_save_failed("Permission denied".into()));
        assert_eq!(state.mark(), Some("non sauvegardé"));
        // La bulle n'est due qu'à la première panne de la série.
        state.on_edit(2);
        assert!(state.due(2));
        assert!(!state.on_save_failed("Permission denied".into()));
        state.on_edit(3);
        state.on_saved("xyz".into());
        assert_eq!(state.mark(), None);
        assert!(state.on_save_failed("again".into()));
    }

    #[test]
    fn a_tab_whose_save_failed_cannot_be_closed_and_keeps_its_mark() {
        let mut state = SaveState::new("x".into());
        // Rien à écrire : la fermeture est libre.
        assert!(!state.blocks_close("x"));
        // Des frappes puis un refus d'écriture à la fermeture : l'onglet reste, marqué.
        state.on_edit(1);
        assert!(state.blocks_close("xy"));
        state.on_save_failed("Permission denied".into());
        assert!(state.blocks_close("xy"));
        // `⌘Z` est revenu au texte du disque : rien à perdre, l'onglet se ferme malgré la panne (ruling 22).
        assert!(!state.blocks_close("x"));
        assert_eq!(state.mark(), Some("non sauvegardé"));
        // Une bulle de plus pour ce geste : la panne n'est « première » qu'une fois, la fermeture la montre quand même.
        assert!(!state.on_save_failed("Permission denied".into()));
        // L'écriture finit par réussir : la fermeture est libre.
        state.on_saved("xy".into());
        assert!(!state.blocks_close("xy"));
    }

    #[test]
    fn a_tab_holds_the_text_the_disk_has_only_when_it_is_the_saved_one() {
        let mut state = SaveState::new("x".into());
        assert!(state.holds("x"));
        assert!(!state.holds("xy"));
        // Des frappes en attente ne changent pas le texte que le disque porte.
        state.on_edit(1);
        assert!(state.holds("x"));
        state.on_saved("xy".into());
        assert!(state.holds("xy"));
        assert!(!state.holds("x"));
    }

    #[test]
    fn an_unreadable_disk_freezes_the_tab_without_a_reload_loop_or_an_overwrite() {
        let mut state = SaveState::new("x".into());
        state.on_edit(1);
        assert!(state.on_unreadable("not UTF-8".into()));
        assert_eq!(state.mark(), Some("illisible"));
        // Les frappes en attente tombent et les suivantes ne sont pas sauvegardées : le fichier illisible reste intact.
        assert!(!state.needs_save());
        state.on_edit(2);
        assert!(!state.due(2));
        assert!(!state.needs_save());
        assert!(!state.blocks_close("y"));
        // Un second échec de la série ne se journalise pas.
        assert!(!state.on_unreadable("not UTF-8".into()));
        // Le fichier redevient du texte : l'onglet le reprend et la marque tombe.
        assert_eq!(
            state.on_disk_change(Some("fresh"), "x"),
            DiskVerdict::Reload {
                pending_edit: false
            }
        );
        assert_eq!(state.mark(), None);
        state.on_edit(3);
        assert!(state.due(3));
    }
}
