//! Le magasin du Scratchpad : ce que le volet sait du fichier de la note, sans fenêtre. Il reprend les règles d'un
//! onglet de Fichiers (`SaveState`, `save_unless_changed`) : une frappe numérotée, une écriture atomique gardée par
//! l'empreinte du fichier, et le disque qui gagne quand quelqu'un d'autre l'a changé (P2-7). Le volet (`panel.rs`)
//! ne fait que brancher les minuteries et l'éditeur sur ces méthodes ; tout se teste donc sur le disque.

use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::files::document::{LineEnding, Opened, Saved, read_document, save_unless_changed};
use crate::files::reference::{agent_reference, selection_lines};
use crate::files::tabs::{DiskVerdict, SaveCheck, SaveState, fingerprint};
use crate::hud;
use crate::scratchpad::rules::FILE_NAME;

/// Ce que « Copier vers l'agent » envoie.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyMode {
    /// La sélection, telle quelle : la note n'a pas de fichier à référencer (le Scratchpad).
    Text,
    /// La même référence que le volet Fichiers (`<chemin>:L<a>` ou `'<chemin>  #L<a>-L<b>'`, suivie d'une espace) : le
    /// fichier est `root` joint au `label` de la note (`AGENTS.md` à la racine du projet).
    Reference { root: PathBuf },
}

/// Ce qui distingue une note d'une autre : son nom dans les messages, son étiquette de journal, la création de son
/// dossier et ce que « Copier vers l'agent » envoie. Le Scratchpad est `NoteOptions::scratchpad()` ; `AGENTS.md` du volet
/// Projet en a d'autres.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteOptions {
    /// Le nom du fichier tel que les messages d'erreur le disent.
    pub label: &'static str,
    /// L'étiquette qui ouvre chaque ligne du journal (`<log_tag> saved`, `<log_tag> reloaded`...).
    pub log_tag: &'static str,
    /// Vrai : le dossier du fichier est créé à la première écriture. Faux : un dossier disparu n'est pas recréé.
    pub create_parent: bool,
    pub copy: CopyMode,
}

impl NoteOptions {
    /// Les valeurs du Scratchpad.
    pub fn scratchpad() -> Self {
        Self {
            label: FILE_NAME,
            log_tag: "scratchpad",
            create_parent: true,
            copy: CopyMode::Text,
        }
    }
}

/// Ce que la relecture du disque décide pour l'éditeur.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Le disque n'a rien de neuf pour l'éditeur (même empreinte, ou même contenu qu'il connaît déjà) : le curseur ne
    /// bouge pas (risque P2-R4).
    Nothing,
    /// Le disque a un autre contenu : l'éditeur le reprend (`text` est en `\n`). `pending_edit` dit que des frappes
    /// attendaient leur écriture et sont perdues, le disque gagne.
    Reload { text: String, pending_edit: bool },
    /// Le fichier n'existe plus. L'éditeur garde son texte : la prochaine frappe recrée le fichier.
    Deleted,
    /// Le disque porte un fichier que l'éditeur ne sait pas lire : le texte reste, plus rien n'est écrit par-dessus.
    Unreadable,
}

/// Ce que rend une écriture (`ScratchpadStore::write`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    /// Le disque porte maintenant le texte de l'éditeur.
    Written,
    /// Rien n'attendait d'être écrit.
    Idle,
    /// L'écriture a été refusée : le texte reste dans l'éditeur, le bandeau dit pourquoi.
    Failed,
    /// Le disque avait changé sous l'écriture : rien n'a été écrasé, le disque a été relu, et `Verdict` est ce que
    /// l'éditeur doit en faire. `Nothing` ici veut dire que le disque a encore bougé pendant la reprise : la prochaine
    /// relecture tranchera.
    Conflict(Verdict),
}

/// Une tentative d'écriture, avant tout traitement d'un conflit.
enum Attempt {
    Written,
    Idle,
    Failed,
    Conflict,
}

pub struct ScratchpadStore {
    path: PathBuf,
    label: &'static str,
    log_tag: &'static str,
    create_parent: bool,
    save: SaveState,
    line_ending: LineEnding,
    /// Le numéro de la dernière frappe : la sauvegarde d'une seconde n'est due que pour elle.
    generation: u64,
    /// Le bandeau d'erreur : une écriture refusée ou un disque illisible. Il tombe à la prochaine écriture réussie ou à
    /// la prochaine lecture du disque.
    notice: Option<String>,
}

impl ScratchpadStore {
    /// Lit la note. Un fichier absent donne un texte vide et ne crée rien : il naît à la première écriture. Un fichier
    /// qui n'est pas du texte lisible donne un texte vide, le bandeau, et n'est jamais réécrit. Rend le magasin et le
    /// texte que l'éditeur doit montrer.
    #[cfg(test)]
    pub fn open(path: PathBuf) -> (Self, String) {
        Self::open_with(path, &NoteOptions::scratchpad())
    }

    /// `open`, pour une note qui a ses propres messages et son propre journal.
    pub fn open_with(path: PathBuf, options: &NoteOptions) -> (Self, String) {
        // L'empreinte avant le contenu : une écriture entre les deux est vue à la relecture suivante.
        let print = fingerprint(&path).ok().flatten();
        let mut line_ending = LineEnding::Lf;
        let mut unreadable = None;
        let text = match read_document(&path) {
            Ok(Opened::Text(loaded)) => {
                line_ending = loaded.line_ending;
                loaded.text
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
            Ok(Opened::External) => {
                unreadable = Some(NOT_TEXT.to_owned());
                String::new()
            }
            Err(error) => {
                unreadable = Some(error.to_string());
                String::new()
            }
        };
        let mut save = SaveState::new(text.clone());
        save.set_fingerprint(print);
        let mut store = Self {
            path,
            label: options.label,
            log_tag: options.log_tag,
            create_parent: options.create_parent,
            save,
            line_ending,
            generation: 0,
            notice: None,
        };
        if let Some(reason) = unreadable {
            store.mark_unreadable(reason);
        }
        (store, text)
    }

    /// Une frappe : rend son numéro, que la minuterie rapporte à `due`.
    pub fn on_edit(&mut self) -> u64 {
        self.generation += 1;
        self.save.on_edit(self.generation);
        self.generation
    }

    /// La minuterie de la frappe `generation` a sonné : est-ce encore la dernière, et le disque ne l'a-t-il pas annulée ?
    pub fn due(&self, generation: u64) -> bool {
        self.save.due(generation)
    }

    /// Le message du bandeau d'erreur, s'il y en a un.
    pub fn banner(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Écrit `text`, le contenu de l'éditeur, s'il y a quelque chose à écrire. Jamais par-dessus un disque qui a changé :
    /// il est alors relu (le disque gagne) et le verdict rendu ; si cette relecture ne trouve rien de neuf (un simple
    /// `touch`), l'écriture repart une fois.
    pub fn write(&mut self, text: &str) -> WriteOutcome {
        match self.attempt(text) {
            Attempt::Written => WriteOutcome::Written,
            Attempt::Idle => WriteOutcome::Idle,
            Attempt::Failed => WriteOutcome::Failed,
            Attempt::Conflict => match self.poll(text) {
                Verdict::Nothing => match self.attempt(text) {
                    Attempt::Written => WriteOutcome::Written,
                    Attempt::Idle => WriteOutcome::Idle,
                    Attempt::Failed => WriteOutcome::Failed,
                    Attempt::Conflict => WriteOutcome::Conflict(Verdict::Nothing),
                },
                verdict => WriteOutcome::Conflict(verdict),
            },
        }
    }

    /// Écrit tout de suite ce qui attend, sans fenêtre pour recharger (`⌘Q`, fermeture du volet) : un disque dont le
    /// contenu a vraiment changé est journalisé et abandonné, le disque gagne, et l'état reste tel quel pour que la
    /// prochaine relecture le reprenne. Une empreinte seule qui a bougé (un `touch`, le même contenu) ne compte pas,
    /// comme dans `write` : l'écriture repart une fois. `reason` est celle du journal (« quit », « hide »). Rend faux
    /// quand le texte n'a pas pu partir.
    pub fn flush(&mut self, text: &str, reason: &str) -> bool {
        let mut outcome = self.attempt(text);
        if matches!(outcome, Attempt::Conflict) && self.disk_content_is_known() {
            outcome = self.attempt(text);
        }
        match outcome {
            Attempt::Written | Attempt::Idle => true,
            Attempt::Failed => {
                hud::log_event(&format!("{} flush failed reason={reason}", self.log_tag));
                false
            }
            Attempt::Conflict => {
                hud::log_event(&format!(
                    "{} flush skipped reason={reason}: changed on disk",
                    self.log_tag
                ));
                false
            }
        }
    }

    /// Le disque porte-t-il encore le texte que le magasin en connaît, malgré une empreinte qui a bougé ? Si oui,
    /// l'empreinte est adoptée sans toucher au reste de l'état (contrairement à `poll`, qui consomme le verdict) et
    /// l'écriture peut repartir. Un fichier absent, illisible ou différent rend faux.
    fn disk_content_is_known(&mut self) -> bool {
        let Ok(print) = fingerprint(&self.path) else {
            return false;
        };
        match read_document(&self.path) {
            Ok(Opened::Text(loaded)) if self.save.holds(&loaded.text) => {
                self.save.set_fingerprint(print);
                true
            }
            _ => false,
        }
    }

    /// Compare le disque à ce que le magasin en sait. Une empreinte identique ne coûte qu'un `stat` ; sinon le fichier est
    /// relu et `SaveState::on_disk_change` tranche. `buffer` est le texte de l'éditeur.
    pub fn poll(&mut self, buffer: &str) -> Verdict {
        let Ok(print) = fingerprint(&self.path) else {
            // Une métadonnée illisible n'est pas un changement : l'écriture dira la vraie panne.
            return Verdict::Nothing;
        };
        if print == self.save.fingerprint() {
            return Verdict::Nothing;
        }
        let disk = match read_document(&self.path) {
            Ok(Opened::Text(loaded)) => Some(loaded),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Ok(Opened::External) => {
                self.save.set_fingerprint(print);
                self.mark_unreadable(NOT_TEXT.to_owned());
                return Verdict::Unreadable;
            }
            Err(error) => {
                self.save.set_fingerprint(print);
                self.mark_unreadable(error.to_string());
                return Verdict::Unreadable;
            }
        };
        if let Some(loaded) = &disk {
            self.line_ending = loaded.line_ending;
        }
        let verdict = self
            .save
            .on_disk_change(disk.as_ref().map(|loaded| loaded.text.as_str()), buffer);
        // Ce que le disque a maintenant est ce que le magasin sait de lui (un fichier supprimé n'a pas d'empreinte).
        if verdict != DiskVerdict::Deleted {
            self.save.set_fingerprint(print);
        }
        self.notice = None;
        match (verdict, disk) {
            (DiskVerdict::Reload { pending_edit }, Some(loaded)) => {
                hud::log_event(&format!(
                    "{} reloaded pending_edit={pending_edit}",
                    self.log_tag
                ));
                Verdict::Reload {
                    text: loaded.text,
                    pending_edit,
                }
            }
            (DiskVerdict::Deleted, _) => {
                hud::log_event(&format!("{} file deleted", self.log_tag));
                Verdict::Deleted
            }
            _ => Verdict::Nothing,
        }
    }

    fn attempt(&mut self, text: &str) -> Attempt {
        if !self.save.needs_save() {
            return Attempt::Idle;
        }
        // Une métadonnée illisible n'est pas un changement : l'écriture dira la vraie panne.
        if let Ok(disk) = fingerprint(&self.path)
            && self.save.check_write(disk) == SaveCheck::DiskChanged
        {
            return Attempt::Conflict;
        }
        // Le dossier de configuration peut manquer au premier lancement : la note le crée avec elle. Un projet supprimé
        // n'est pas recréé : l'écriture échoue et le bandeau le dit.
        if self.create_parent
            && let Some(parent) = self.path.parent()
        {
            let _ = std::fs::create_dir_all(parent);
        }
        // L'empreinte est relue une dernière fois juste avant le `rename` : un agent qui écrit entre la vérification
        // ci-dessus et le remplacement n'est pas écrasé.
        match save_unless_changed(&self.path, text, self.line_ending, self.save.fingerprint()) {
            Ok(Saved::Written) => {
                self.save.on_saved(text.to_owned());
                self.save
                    .set_fingerprint(fingerprint(&self.path).ok().flatten());
                self.notice = None;
                hud::log_event(&format!("{} saved", self.log_tag));
                Attempt::Written
            }
            Ok(Saved::DiskChanged) => Attempt::Conflict,
            Err(error) => {
                self.save.on_save_failed(error.to_string());
                // Le bandeau parle français ; l'erreur système brute reste dans le journal seulement.
                self.notice = Some(write_failure_message(self.label, error.kind()));
                hud::log_event(&format!("{} save failed reason={error}", self.log_tag));
                Attempt::Failed
            }
        }
    }

    fn mark_unreadable(&mut self, reason: String) {
        let first = self.save.on_unreadable(reason.clone());
        self.notice = Some(format!("Lecture impossible : {reason}"));
        if first {
            hud::log_event(&format!("{} read failed reason={reason}", self.log_tag));
        }
    }
}

/// Le texte du bandeau pour une écriture refusée : la cause en une phrase courte, sans le message brut du système
/// (« Permission denied (os error 13) »), qui va au journal.
fn write_failure_message(label: &str, kind: io::ErrorKind) -> String {
    match kind {
        io::ErrorKind::PermissionDenied => {
            format!("Écriture impossible : droits insuffisants sur {label}")
        }
        io::ErrorKind::StorageFull => "Écriture impossible : disque plein".to_owned(),
        _ => "Écriture impossible : erreur système".to_owned(),
    }
}

/// Le texte du bandeau pour le Scratchpad : `write_failure_message` avec le nom de sa note.
#[cfg(test)]
fn write_failure_copy(kind: io::ErrorKind) -> String {
    write_failure_message(FILE_NAME, kind)
}

/// Pourquoi un fichier que l'éditeur ne sait pas ouvrir est refusé (binaire, non UTF-8, plus de 5 Mo).
const NOT_TEXT: &str = "ce n'est pas du texte lisible (binaire, non UTF-8 ou plus de 5 Mo)";

/// Le texte à envoyer à l'agent pour la sélection `range` : tel quel, sans espace ni retour à la ligne de plus. `None`
/// quand rien n'est sélectionné, ou quand la plage ne tombe pas sur des frontières de caractère du texte.
pub fn selection_payload(text: &str, range: Range<usize>) -> Option<String> {
    // Le sens de la sélection ne compte pas : une sélection faite de droite à gauche a son début après sa fin.
    let start = range.start.min(range.end);
    let end = range.start.max(range.end);
    if start == end {
        return None;
    }
    text.get(start..end).map(str::to_owned)
}

/// Ce que « Copier vers l'agent » envoie pour la sélection `selection` de `text`. `Text` : la sélection telle quelle, rien
/// sans sélection. `Reference` : la référence du fichier, aux lignes de la sélection ou à celle du curseur quand rien n'est
/// sélectionné, suivie de l'espace que Fichiers ajoute aussi ; `home` raccourcit le chemin en `~/`.
pub fn copy_payload(
    options: &NoteOptions,
    text: &str,
    selection: Range<usize>,
    home: Option<&Path>,
) -> Option<String> {
    match &options.copy {
        CopyMode::Text => selection_payload(text, selection),
        CopyMode::Reference { root } => {
            // Le sens de la sélection ne compte pas : `selection_lines` veut le début avant la fin.
            let ordered = selection.start.min(selection.end)..selection.start.max(selection.end);
            let lines = selection_lines(text, ordered);
            let file = root.join(options.label);
            Some(format!("{} ", agent_reference(&file, home, Some(lines))))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scratchpad::rules::{clamp_offset, scratchpad_path};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, SystemTime};

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("idema-scratchpad-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// La note déjà sur le disque avec `content`, et le magasin qui l'a ouverte.
    fn opened_with(name: &str, content: &str) -> (ScratchpadStore, PathBuf) {
        let path = scratchpad_path(&temp_dir(name));
        fs::write(&path, content).unwrap();
        let (store, _) = ScratchpadStore::open(path.clone());
        (store, path)
    }

    #[test]
    fn opening_never_creates_the_file() {
        let dir = temp_dir("opening");
        let path = scratchpad_path(&dir);
        let (mut store, text) = ScratchpadStore::open(path.clone());
        assert_eq!(text, "");
        assert!(!path.exists());
        // Ni une relecture ni une écriture sans frappe n'en créent un.
        assert_eq!(store.poll(""), Verdict::Nothing);
        assert_eq!(store.write(""), WriteOutcome::Idle);
        assert!(store.flush("", "quit"));
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        assert!(!dir.join(FILE_NAME).exists());
    }

    #[test]
    fn an_edit_is_on_disk_after_the_autosave_delay() {
        let dir = temp_dir("autosave");
        let path = scratchpad_path(&dir);
        let (mut store, _) = ScratchpadStore::open(path.clone());
        let first = store.on_edit();
        let last = store.on_edit();
        // Seule la dernière frappe déclenche l'écriture quand sa minuterie sonne.
        assert!(!store.due(first));
        assert!(store.due(last));
        assert_eq!(store.write("abc"), WriteOutcome::Written);
        assert_eq!(fs::read_to_string(&path).unwrap(), "abc");
        assert!(!store.save.needs_save());
    }

    #[test]
    fn the_first_write_creates_a_missing_config_dir() {
        let dir = temp_dir("missing-dir").join("nested");
        let path = scratchpad_path(&dir);
        let (mut store, _) = ScratchpadStore::open(path.clone());
        store.on_edit();
        assert_eq!(store.write("x"), WriteOutcome::Written);
        assert_eq!(fs::read_to_string(&path).unwrap(), "x");
    }

    #[test]
    fn flush_writes_at_once() {
        let dir = temp_dir("flush");
        let path = scratchpad_path(&dir);
        let (mut store, _) = ScratchpadStore::open(path.clone());
        store.on_edit();
        assert!(store.flush("x", "quit"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "x");
    }

    #[test]
    fn flush_leaves_a_changed_disk_alone() {
        let (mut store, path) = opened_with("flush-conflict", "a");
        store.on_edit();
        fs::write(&path, "written by an agent").unwrap();
        assert!(!store.flush("a!", "quit"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "written by an agent");
        // Le disque gagne aussi à la relecture suivante : la frappe perdue est rendue au verdict, pas écrite.
        assert_eq!(
            store.poll("a!"),
            Verdict::Reload {
                text: "written by an agent".to_owned(),
                pending_edit: true
            }
        );
    }

    #[test]
    fn flush_keeps_the_pending_edit_when_only_the_fingerprint_changed() {
        let (mut store, path) = opened_with("flush-touch", "a");
        store.on_edit();
        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_modified(SystemTime::now() + Duration::from_secs(30))
            .unwrap();
        // Un `touch` au contenu identique n'est pas un conflit : le texte en attente part avant la sortie.
        assert!(store.flush("a!", "quit"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "a!");
        assert!(!store.save.needs_save());
    }

    #[test]
    fn a_disk_change_reloads_the_editor() {
        let (mut store, path) = opened_with("reload", "a");
        assert_eq!(store.poll("a"), Verdict::Nothing);
        fs::write(&path, "bravo").unwrap();
        assert_eq!(
            store.poll("a"),
            Verdict::Reload {
                text: "bravo".to_owned(),
                pending_edit: false
            }
        );
        // Le disque est maintenant connu : la relecture suivante ne rend plus rien.
        assert_eq!(store.poll("bravo"), Verdict::Nothing);
    }

    #[test]
    fn a_crlf_note_keeps_its_line_endings_on_save() {
        let (mut store, path) = opened_with("crlf", "a\r\nb\r\n");
        store.on_edit();
        assert_eq!(store.write("a\nb\nc\n"), WriteOutcome::Written);
        assert_eq!(fs::read(&path).unwrap(), b"a\r\nb\r\nc\r\n");
    }

    #[test]
    fn a_touch_without_new_content_changes_nothing() {
        let (mut store, path) = opened_with("touch", "same");
        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_modified(SystemTime::now() + Duration::from_secs(30))
            .unwrap();
        // Le même contenu sous une autre date : pas de rechargement, le curseur ne bouge pas.
        assert_eq!(store.poll("same"), Verdict::Nothing);
    }

    #[test]
    fn the_disk_wins_over_a_pending_edit() {
        let (mut store, path) = opened_with("disk-wins", "a");
        store.on_edit();
        fs::write(&path, "zzz").unwrap();
        // L'autosave sonne : elle n'écrase pas le disque, elle le reprend.
        assert_eq!(
            store.write("a!"),
            WriteOutcome::Conflict(Verdict::Reload {
                text: "zzz".to_owned(),
                pending_edit: true
            })
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "zzz");
        // La frappe est perdue pour de bon : rien ne se réécrit par-dessus.
        assert!(!store.save.needs_save());
        assert_eq!(store.write("zzz"), WriteOutcome::Idle);
        assert_eq!(fs::read_to_string(&path).unwrap(), "zzz");
    }

    #[test]
    fn a_touched_disk_does_not_lose_the_pending_edit() {
        let (mut store, path) = opened_with("touch-pending", "a");
        store.on_edit();
        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_modified(SystemTime::now() + Duration::from_secs(30))
            .unwrap();
        // L'empreinte a changé mais pas le contenu : la relecture n'a rien à reprendre, l'écriture repart.
        assert_eq!(store.write("a!"), WriteOutcome::Written);
        assert_eq!(fs::read_to_string(&path).unwrap(), "a!");
    }

    #[test]
    fn reload_clamps_the_cursor_to_the_new_text() {
        let long = "x".repeat(200);
        let (mut store, path) = opened_with("clamp", &long);
        let cursor = long.len();
        fs::write(&path, "ok").unwrap();
        let Verdict::Reload { text, .. } = store.poll(&long) else {
            panic!("the disk change must reload the editor");
        };
        assert_eq!(text, "ok");
        assert_eq!(clamp_offset(cursor, &text), 2);
    }

    #[test]
    fn a_deleted_file_is_recreated_by_the_next_edit() {
        let (mut store, path) = opened_with("deleted", "a");
        fs::remove_file(&path).unwrap();
        assert_eq!(store.poll("a"), Verdict::Deleted);
        // Rien ne recrée le fichier tant que l'utilisateur ne tape pas.
        assert_eq!(store.poll("a"), Verdict::Nothing);
        assert_eq!(store.write("a"), WriteOutcome::Idle);
        assert!(!path.exists());
        store.on_edit();
        assert_eq!(store.write("ab"), WriteOutcome::Written);
        assert_eq!(fs::read_to_string(&path).unwrap(), "ab");
    }

    #[test]
    fn a_refused_write_keeps_the_text_and_shows_the_error() {
        let (mut store, path) = opened_with("refused", "a");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
        store.on_edit();
        assert_eq!(store.write("ab"), WriteOutcome::Failed);
        let banner = store.banner().expect("the refusal must show a banner");
        assert_eq!(
            banner,
            "Écriture impossible : droits insuffisants sur scratchpad.md"
        );
        // L'erreur système brute n'est que dans le journal.
        assert!(!banner.contains("os error"), "{banner}");
        // Le disque est intact, et l'écriture refusée reste à rejouer.
        assert_eq!(fs::read_to_string(&path).unwrap(), "a");
        assert!(store.save.needs_save());
        // Le fichier redevient inscriptible : la reprise écrit et efface le bandeau.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(store.write("ab"), WriteOutcome::Written);
        assert_eq!(store.banner(), None);
        assert_eq!(fs::read_to_string(&path).unwrap(), "ab");
    }

    #[test]
    fn each_write_failure_kind_has_french_copy_without_the_system_text() {
        assert_eq!(
            write_failure_copy(io::ErrorKind::StorageFull),
            "Écriture impossible : disque plein"
        );
        assert_eq!(
            write_failure_copy(io::ErrorKind::ReadOnlyFilesystem),
            "Écriture impossible : erreur système"
        );
    }

    #[test]
    fn an_unreadable_file_is_never_overwritten() {
        let path = scratchpad_path(&temp_dir("unreadable"));
        let binary = [0xff, 0xfe, 0x00, 0x01];
        fs::write(&path, binary).unwrap();
        let (mut store, text) = ScratchpadStore::open(path.clone());
        assert_eq!(text, "");
        assert!(
            store
                .banner()
                .is_some_and(|banner| banner.starts_with("Lecture impossible"))
        );
        store.on_edit();
        assert_eq!(store.write("typed"), WriteOutcome::Idle);
        // Rien n'attend d'être écrit : le vidage de sortie laisse le fichier tel quel.
        assert!(store.flush("typed", "quit"));
        assert_eq!(fs::read(&path).unwrap(), binary);
        // Le fichier redevient du texte : l'éditeur le reprend et le bandeau tombe.
        fs::write(&path, "readable again").unwrap();
        assert_eq!(
            store.poll(""),
            Verdict::Reload {
                text: "readable again".to_owned(),
                pending_edit: false
            }
        );
        assert_eq!(store.banner(), None);
    }

    #[test]
    fn copy_to_agent_sends_the_selection_verbatim() {
        let text = "l1\nl2\nl3";
        assert_eq!(
            selection_payload(text, 0..text.len()),
            Some("l1\nl2\nl3".to_owned())
        );
        // Sans espace ni retour à la ligne de plus, même quand la sélection finit sur un saut de ligne.
        assert_eq!(selection_payload(text, 0..3), Some("l1\n".to_owned()));
        // Une sélection de droite à gauche donne le même texte.
        // La plage inversée est voulue : un littéral `5..3` serait refusé par clippy (`reversed_empty_ranges`).
        let reversed = Range { start: 5, end: 3 };
        assert_eq!(selection_payload(text, reversed), Some("l2".to_owned()));
        // Sans sélection : rien ne part.
        assert_eq!(selection_payload(text, 4..4), None);
        // Une plage hors du texte ou au milieu d'un caractère ne fait pas paniquer.
        assert_eq!(selection_payload(text, 0..99), None);
        assert_eq!(selection_payload("é", 0..1), None);
    }
    /// Les options d'`AGENTS.md` dans le volet Projet.
    fn agents_options(root: &Path) -> NoteOptions {
        NoteOptions {
            label: "AGENTS.md",
            log_tag: "instructions",
            create_parent: false,
            copy: CopyMode::Reference {
                root: root.to_path_buf(),
            },
        }
    }

    #[test]
    fn a_note_that_does_not_create_its_parent_fails_instead_of_recreating_a_deleted_folder() {
        let project = temp_dir("no-parent").join("gone");
        let path = project.join("AGENTS.md");
        let (mut store, _) = ScratchpadStore::open_with(path.clone(), &agents_options(&project));
        store.on_edit();
        assert_eq!(store.write("rules"), WriteOutcome::Failed);
        assert!(!project.exists(), "the deleted project folder came back");
        assert_eq!(store.banner(), Some("Écriture impossible : erreur système"));
    }

    #[test]
    fn the_scratchpad_keeps_creating_its_parent() {
        let options = NoteOptions::scratchpad();
        assert!(options.create_parent);
        assert_eq!(options.label, FILE_NAME);
        assert_eq!(options.copy, CopyMode::Text);
    }

    #[test]
    fn a_write_failure_names_the_note_it_belongs_to() {
        assert_eq!(
            write_failure_message("AGENTS.md", io::ErrorKind::PermissionDenied),
            "Écriture impossible : droits insuffisants sur AGENTS.md"
        );
        assert_eq!(
            write_failure_copy(io::ErrorKind::PermissionDenied),
            "Écriture impossible : droits insuffisants sur scratchpad.md"
        );
    }

    #[test]
    fn a_reference_copy_sends_the_file_reference_with_or_without_a_selection() {
        let root = Path::new("/Users/me/dev/app");
        let home = Path::new("/Users/me");
        let options = agents_options(root);
        let text = "l1\nl2\nl3";
        // Le curseur seul (rien de sélectionné) : la ligne du curseur, comme Fichiers.
        assert_eq!(
            copy_payload(&options, text, 4..4, Some(home)),
            Some("~/dev/app/AGENTS.md:L2 ".to_owned())
        );
        // Plusieurs lignes : la plage, entre apostrophes.
        assert_eq!(
            copy_payload(&options, text, 0..8, Some(home)),
            Some("'~/dev/app/AGENTS.md  #L1-L3' ".to_owned())
        );
        // Le Scratchpad garde son comportement : la sélection telle quelle, rien sans sélection.
        let note = NoteOptions::scratchpad();
        assert_eq!(copy_payload(&note, text, 0..2, None), Some("l1".to_owned()));
        assert_eq!(copy_payload(&note, text, 4..4, None), None);
    }
}
