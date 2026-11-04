//! Les gestes de l'arbre du volet Fichiers : appui, clic, menu, création, renommage, glisser-déposer et Corbeille.

use super::*;

use super::tree_view::DraggedEntry;
use crate::files::gestures::TreeGesture;
use crate::files::ops::{
    MovePlan, NameError, OpError, create_dir, create_file, move_entry, move_plan, move_to_trash,
    rename_entry, stem_len,
};
use crate::files::reference::display_path;
use crate::files::tree_edit::{EditKind, EditSlot};
use crate::rename_field::{FieldOptions, RenameEvent, RenameField};
use gpui::ClipboardItem;
use std::ops::Range;

/// Le refus de nom affiché après un `Changed` du champ. Seul un texte qui change l'efface : un `⌘⌫` ou une chasse arrière
/// sans effet émet aussi `Changed`, avec le même texte, et le message reste. `last_text` garde le dernier texte vu.
fn error_after_change(
    error: Option<NameError>,
    last_text: &mut String,
    text: &str,
) -> Option<NameError> {
    if last_text == text {
        return error;
    }
    text.clone_into(last_text);
    None
}

/// Le dossier qu'un dépôt éclaire : le candidat sous la souris, sauf si `source` n'y irait nulle part (déjà dans ce
/// dossier, ou un dossier qu'on rangerait en lui-même ou dessous). Rien n'est éclairé pour un dépôt qui ne ferait rien.
fn highlighted_target(source: &Path, candidate: Option<PathBuf>) -> Option<PathBuf> {
    candidate.filter(|dir| matches!(move_plan(source, dir), MovePlan::To(_)))
}

/// Le champ ouvert dans l'arbre, pour créer ou renommer. Il tient la place de la ligne (ou s'insère dans le dossier qui
/// reçoit) tant que `slot` le décrit ; `list_items` dit où le dessiner.
pub(super) struct TreeEdit {
    pub(super) slot: EditSlot,
    pub(super) field: Entity<RenameField>,
    /// Le dernier texte rendu par le champ : seul un texte qui change efface un refus. Un `⌘⌫` ou une chasse arrière sans
    /// effet émet aussi `Changed`, avec le même texte.
    last_text: String,
    _subscription: Subscription,
}

impl FilesPanel {
    /// Un appui gauche sur une ligne : l'arbre prend le focus et la ligne est sélectionnée, rien d'autre (écart E45) :
    /// déplier ou ouvrir attend le relâché, pour qu'un glisser ne commence pas par ouvrir ce qu'il déplace.
    pub(super) fn on_row_pressed(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(self.tree_focus_handle(), cx);
        // Le corps du volet reprend le focus à la remontée d'un appui, sauf s'il est empêché.
        window.prevent_default();
        self.selected = Some(path.to_path_buf());
        cx.notify();
    }

    /// Un clic sur une ligne, au relâché (écart E45). Un dossier se plie ou se déplie (la première lecture d'un dossier
    /// fait redemander à Git ce qui est ignoré, pour que ce qu'on y a créé plus tôt soit atténué). Un fichier s'ouvre en
    /// onglet d'aperçu, et le double-clic le rend permanent (spec §4.2).
    pub(super) fn on_row_clicked(
        &mut self,
        path: &Path,
        is_dir: bool,
        click_count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected = Some(path.to_path_buf());
        if is_dir {
            let first_read = !self.tree.is_loaded(path);
            match self.tree.toggle(path) {
                Ok(()) if first_read => self.refresh_ignored(cx),
                Ok(()) => {}
                Err(error) => self.report_read_errors(vec![(path.to_path_buf(), error)]),
            }
        } else {
            // Un clic simple ouvre l'aperçu et reste dans l'arbre (↩ et ⌘⌫ y jouent) ; le double-clic épingle et passe à
            // l'éditeur.
            let double = click_count >= 2;
            let already_open = self.tabs.index_of(&canonical_path(path)).is_some();
            if !is_repeat_of_external_open(click_count, already_open) {
                self.open_file(path, None, double, window, cx);
                if double {
                    self.pin_active(cx);
                }
            }
        }
        self.rebuild_rows();
        cx.notify();
    }

    /// Un appui droit : l'arbre prend le focus, la ligne est sélectionnée (la racine n'a pas de ligne : plus rien ne
    /// l'est, pour que `⌘⌫` ne vise pas une ligne que le menu ne montrait pas), puis `Workspace` ouvre le menu.
    pub(super) fn on_tree_menu_requested(
        &mut self,
        target: TreeTarget,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(self.tree_focus_handle(), cx);
        window.prevent_default();
        self.selected = match &target {
            TreeTarget::Root => None,
            TreeTarget::Entry { path, .. } => Some(path.clone()),
        };
        cx.emit(FilesEvent::TreeMenu { target, position });
        cx.notify();
    }

    /// Exécute un geste du menu de l'arbre sur `target` (le menu contextuel, `↩` et `⌘⌫` y passent tous).
    pub fn run_tree_gesture(
        &mut self,
        gesture: TreeGesture,
        target: &TreeTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = target.path(&self.root).to_path_buf();
        match gesture {
            TreeGesture::NewFile => self.start_creation(EditKind::NewFile, target, window, cx),
            TreeGesture::NewFolder => self.start_creation(EditKind::NewFolder, target, window, cx),
            TreeGesture::Rename => self.start_rename(target, window, cx),
            TreeGesture::Trash => self.trash(target, window, cx),
            TreeGesture::OpenDefault => {
                // Un dossier est révélé, jamais ouvert (écart E48) : le menu grise l'entrée, ceci garde le geste.
                if matches!(target, TreeTarget::Entry { is_dir: false, .. }) {
                    self.open_externally(&path, window, cx);
                }
            }
            TreeGesture::Reveal => {
                let outcome = reveal_in_finder(&path);
                self.alert_on_failure(&path, outcome, "Impossible de révéler", window, cx);
            }
            TreeGesture::Ignore => {
                // La racine ne s'ignore pas, et une entrée suivie ou déjà ignorée non plus : le menu grise l'entrée, ceci
                // garde le geste.
                if let TreeTarget::Entry { is_dir, .. } = target {
                    let git = self.entry_git(&path);
                    if !git.tracked && !git.ignored {
                        // Git ne fait pas correspondre `lien/` à un lien : un dossier-lien s'écrit comme un fichier.
                        let is_dir = *is_dir && !path.is_symlink();
                        self.ignore_entry(path, is_dir, window, cx);
                    }
                }
            }
            TreeGesture::CopyPath => {
                let shown = display_path(&path, home_dir().as_deref());
                cx.write_to_clipboard(ClipboardItem::new_string(shown));
                hud::log_event(&format!("files path copied path={}", path.display()));
            }
            TreeGesture::CopyToAgent => {
                let reference = agent_reference(&path, home_dir().as_deref(), None);
                cx.emit(FilesEvent::CopyToAgent(reference));
            }
        }
    }

    /// « Ajouter au .gitignore » : la ligne s'écrit à la racine du dépôt (celle que donne `rev-parse --show-toplevel`, le
    /// projet pouvant n'en être qu'un sous-dossier), hors du fil d'interface. Réussi, le journal dit la ligne et l'arbre
    /// relit ce que Git ignore (l'entrée s'atténue) ; refusé, l'alerte dit pourquoi, avec le message brut.
    fn ignore_entry(
        &mut self,
        path: PathBuf,
        is_dir: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let root = self.root.clone();
        hud::log_event("changes gesture ignore paths=1");
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    crate::changes::ignore::add_path_to_gitignore(&root, &path, is_dir)
                })
                .await;
            let _ = this.update_in(cx, |panel, window, cx| match result {
                Ok(outcome) => {
                    outcome.log();
                    panel.refresh_ignored(cx);
                }
                Err(error) => alert(
                    window,
                    cx,
                    crate::changes::ignore::WRITE_FAILED,
                    Some(&error.to_string()),
                ),
            });
        })
        .detach();
    }

    /// `↩` et `⌘⌫` : le geste vise la ligne sélectionnée, si elle est à l'écran.
    pub(super) fn run_on_selected(
        &mut self,
        gesture: TreeGesture,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self.selected.clone() else {
            return;
        };
        let Some(row) = self.rows.iter().find(|row| row.path == path) else {
            return;
        };
        let target = TreeTarget::Entry {
            path,
            is_dir: row.is_dir,
        };
        self.run_tree_gesture(gesture, &target, window, cx);
    }

    /// « Nouveau fichier » et « Nouveau dossier » : le dossier visé se déplie (la racine est toujours ouverte) et le
    /// champ s'ouvre en tête, vide.
    fn start_creation(
        &mut self,
        kind: EditKind,
        target: &TreeTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dir = target.dir(&self.root);
        let first_read = !self.tree.is_loaded(&dir);
        if let Err(failure) = self.tree.reveal(&dir) {
            self.report_read_errors(vec![failure]);
            return;
        }
        if first_read {
            self.refresh_ignored(cx);
        }
        self.open_field(kind, dir, "", None, window, cx);
    }

    /// « Renommer » : le champ prend la place de la ligne, le nom sans son extension sélectionné.
    fn start_rename(&mut self, target: &TreeTarget, window: &mut Window, cx: &mut Context<Self>) {
        let TreeTarget::Entry { path, is_dir } = target else {
            return;
        };
        let Some(name) = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        else {
            return;
        };
        if !self.rows.iter().any(|row| row.path == *path) {
            return;
        }
        self.selected = Some(path.clone());
        let stem = stem_len(&name, *is_dir);
        self.open_field(
            EditKind::Rename,
            path.clone(),
            &name,
            Some(0..stem),
            window,
            cx,
        );
    }

    /// Ouvre le champ de l'arbre (il prend le focus) : `anchor` est le dossier qui reçoit, ou l'entrée renommée. Un
    /// champ déjà ouvert est remplacé.
    fn open_field(
        &mut self,
        kind: EditKind,
        anchor: PathBuf,
        initial: &str,
        select: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let field = rename_field::open_with(initial, FieldOptions::FILE_NAME, window, cx);
        if let Some(range) = select {
            field.update(cx, |field, cx| field.select_range(range, cx));
        }
        let subscription = cx.subscribe_in(
            &field,
            window,
            |this, field, event: &RenameEvent, window, cx| {
                this.on_edit_event(field, event, window, cx);
            },
        );
        self.edit = Some(TreeEdit {
            slot: EditSlot {
                kind,
                anchor,
                error: None,
            },
            field,
            last_text: initial.to_owned(),
            _subscription: subscription,
        });
        self.rebuild_rows();
        if let Some(index) = self
            .items
            .iter()
            .position(|item| matches!(item, ListItem::Field { .. }))
        {
            self.scroll.scroll_to_item(index, ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    /// Ferme le champ. `refocus` rend le focus à l'arbre (Échap, réussite) ; une perte du focus l'a déjà donné ailleurs.
    fn close_edit(&mut self, refocus: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.edit = None;
        self.rebuild_rows();
        if refocus {
            self.focus_tree(window, cx);
        }
        cx.notify();
    }

    fn on_edit_event(
        &mut self,
        field: &Entity<RenameField>,
        event: &RenameEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(edit) = self.edit.as_mut().filter(|edit| edit.field == *field) else {
            return;
        };
        match event {
            RenameEvent::Changed(text) => {
                let shown = edit.slot.error;
                edit.slot.error = error_after_change(shown, &mut edit.last_text, text);
                if shown.is_some() && edit.slot.error.is_none() {
                    self.rebuild_rows();
                    cx.notify();
                }
            }
            RenameEvent::Cancel => {
                let focused = field.focus_handle(cx).is_focused(window);
                self.close_edit(focused, window, cx);
            }
            RenameEvent::Confirm(name) => self.confirm_edit(name, window, cx),
        }
    }

    /// `↩` dans le champ. Un refus de nom (vide compris : « Nom invalide. ») laisse le champ ouvert, avec son texte, et pose
    /// le message dessous ; une panne du disque ferme le champ et ouvre la bulle.
    fn confirm_edit(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.edit.as_ref() else {
            return;
        };
        let (kind, anchor) = (edit.slot.kind, edit.slot.anchor.clone());
        let result = match kind {
            EditKind::NewFile => create_file(&anchor, name),
            EditKind::NewFolder => create_dir(&anchor, name),
            EditKind::Rename => {
                // Le texte frappé il y a moins d'une seconde part d'abord, sous l'ancien chemin : l'onglet le garde en
                // changeant de chemin et la garde du disque reste juste (l'empreinte ne bouge pas à un renommage).
                for tab in self.tabs.paths_under(&tab_key(&anchor)) {
                    self.save_tab(&tab, None, cx);
                }
                rename_entry(&anchor, name)
            }
        };
        match result {
            Ok(path) if kind == EditKind::Rename => {
                self.edit = None;
                if path != anchor {
                    self.follow_rename(&anchor, &path, window, cx);
                    hud::log_event(&format!(
                        "file renamed from={} to={}",
                        anchor.display(),
                        path.display()
                    ));
                }
                self.selected = Some(path);
                self.rebuild_rows();
                self.focus_tree(window, cx);
                cx.notify();
            }
            Ok(path) => self.finish_creation(kind, path, window, cx),
            Err(OpError::Name(refusal)) => self.refuse_name(refusal, cx),
            Err(OpError::Io(error)) => {
                hud::log_event(&format!(
                    "file edit failed path={}: {error}",
                    anchor.display()
                ));
                self.close_edit(true, window, cx);
                let Explained { text, detail } = explain_write(&error);
                alert(window, cx, &text, detail.as_deref());
            }
        }
    }

    /// Le nom est refusé : le champ reste ouvert avec son texte, le message s'écrit sous lui.
    fn refuse_name(&mut self, refusal: NameError, cx: &mut Context<Self>) {
        if let Some(edit) = self.edit.as_mut() {
            edit.slot.error = Some(refusal);
        }
        self.rebuild_rows();
        cx.notify();
    }

    /// Le fichier ou le dossier est créé : le champ se ferme, le dossier parent est relu, la nouvelle ligne est
    /// sélectionnée et à l'écran. Un fichier s'ouvre épinglé, le clavier à l'éditeur ; un dossier garde le focus dans
    /// l'arbre.
    fn finish_creation(
        &mut self,
        kind: EditKind,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.edit = None;
        let outcome = self.tree.refresh(std::slice::from_ref(&path));
        self.report_read_errors(outcome.errors);
        self.refresh_ignored(cx);
        self.selected = Some(path.clone());
        self.rebuild_rows();
        self.scroll_to_row(&path);
        let created = if kind == EditKind::NewFolder {
            "folder"
        } else {
            "file"
        };
        hud::log_event(&format!("{created} created path={}", path.display()));
        if kind == EditKind::NewFolder {
            self.focus_tree(window, cx);
        } else {
            self.open_file(&path, None, true, window, cx);
            self.pin_active(cx);
        }
        cx.notify();
    }

    /// `from` a été renommé ou déplacé en `to` sur le disque : les onglets dessous suivent (clé de la table, titre,
    /// abonnement de l'éditeur ; l'empreinte reste, un renommage ne change ni la taille ni la date), l'arbre garde ses
    /// dossiers dépliés, la sélection suit, et les deux dossiers parents sont relus.
    fn follow_rename(
        &mut self,
        from: &Path,
        to: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let home = home_dir();
        // La cible n'existait pas (le renommage refuse d'écraser) : seuls des onglets « supprimé » peuvent y rester, et
        // ils ne doivent pas doubler l'onglet qui arrive sur leur chemin.
        for gone in self.tabs.close_under(&tab_key(to)) {
            self.open.remove(gone.as_path());
        }
        for (old, new) in self.tabs.rename_under(&tab_key(from), &tab_key(to)) {
            let Some(mut tab) = self.open.remove(old.as_path()) else {
                continue;
            };
            tab.title = tab_title(&new, &self.root, home.as_deref());
            // Une extension qui change (`.txt` vers `.md`) change la coloration : l'éditeur la reprend.
            if language_for(&old) != language_for(&new) {
                let language = language_for(&new).unwrap_or("text");
                tab.editor
                    .update(cx, |editor, cx| editor.set_highlighter(language, cx));
            }
            // Une lecture en cours rendrait compte sous l'ancien chemin et n'effacerait jamais `reloading` : elle repart
            // sous le nouveau.
            let was_reloading = std::mem::take(&mut tab.reloading);
            tab.reload_redo = false;
            tab._reload_task = None;
            tab._subscription = Self::subscribe_editor(&tab.editor, &new, window, cx);
            self.open.insert(TabKey(new.clone()), tab);
            if was_reloading {
                self.reload_tab(&new, window, cx);
            }
        }
        self.tree.rename_under(from, to);
        if let Some(rest) = self
            .selected
            .as_ref()
            .and_then(|selected| selected.strip_prefix(from).ok())
        {
            self.selected = Some(if rest.as_os_str().is_empty() {
                to.to_path_buf()
            } else {
                to.join(rest)
            });
        }
        let outcome = self.tree.refresh(&[from.to_path_buf(), to.to_path_buf()]);
        self.report_read_errors(outcome.errors);
        self.refresh_ignored(cx);
        self.rebuild_rows();
        self.reveal_active_tab();
        cx.notify();
    }

    /// La capture du mouvement est finie : le candidat devient le dossier éclairé, sauf si le dépôt ne ferait rien. Ne
    /// redessine que sur un changement.
    pub(super) fn settle_drop_target(&mut self, source: &Path, cx: &mut Context<Self>) {
        let next = highlighted_target(source, self.drop_candidate.take());
        if next != self.drop_target {
            self.drop_target = next;
            cx.notify();
        }
    }

    /// Dépôt d'une entrée glissée sur `target_dir` : les onglets dessous partent d'abord sur le disque (la frappe d'il y
    /// a moins d'une seconde suit le fichier), puis l'entrée change de dossier sans jamais en écraser une autre. Un
    /// dépôt qui ne ferait rien (même dossier, dossier dans lui-même) ne touche à rien.
    pub(super) fn move_dragged(
        &mut self,
        entry: &DraggedEntry,
        target_dir: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.drop_target = None;
        self.drop_candidate = None;
        cx.notify();
        if !matches!(move_plan(&entry.path, target_dir), MovePlan::To(_)) {
            return;
        }
        for tab in self.tabs.paths_under(&tab_key(&entry.path)) {
            self.save_tab(&tab, None, cx);
        }
        match move_entry(&entry.path, target_dir) {
            Ok(Some(dest)) => {
                // `follow_rename` fait suivre la sélection (la ligne saisie l'est), relit les parents et refait les lignes.
                self.follow_rename(&entry.path, &dest, window, cx);
                self.scroll_to_row(&dest);
                hud::log_event(&format!(
                    "file moved from={} to={}",
                    entry.path.display(),
                    dest.display()
                ));
            }
            Ok(None) => {}
            Err(OpError::Name(refusal)) => alert(window, cx, refusal.message(), None),
            Err(OpError::Io(error)) => {
                hud::log_event(&format!(
                    "file move failed from={} error={error}",
                    entry.path.display()
                ));
                let Explained { text, detail } = explain_write(&error);
                alert(window, cx, &text, detail.as_deref());
            }
        }
    }

    /// « Supprimer » : les onglets sous le chemin partent d'abord sur le disque, puis l'entrée va à la Corbeille (jamais
    /// de suppression définitive). Ses onglets se ferment sans la garde du ruling 21 : le geste est explicite et la
    /// Corbeille garde la dernière version écrite ; si du texte n'a pas pu être écrit, la bulle le dit (écart E49).
    fn trash(&mut self, target: &TreeTarget, window: &mut Window, cx: &mut Context<Self>) {
        let TreeTarget::Entry { path, .. } = target else {
            return;
        };
        let tabs = self.tabs.paths_under(&tab_key(path));
        for tab in &tabs {
            self.save_tab(tab, None, cx);
        }
        let unsaved: Vec<String> = tabs
            .iter()
            .filter(|tab| self.keeps_unsaved_text(tab, cx))
            .filter_map(|tab| self.open.get(tab.as_path()).map(|open| open.title.clone()))
            .collect();
        match move_to_trash(path) {
            Ok(landed) => {
                for tab in &tabs {
                    if let Some(index) = self.tabs.index_of(tab) {
                        self.tabs.close(index);
                    }
                    self.open.remove(tab.as_path());
                }
                self.selected = None;
                let outcome = self.tree.refresh(std::slice::from_ref(path));
                self.report_read_errors(outcome.errors);
                self.refresh_ignored(cx);
                self.rebuild_rows();
                self.reveal_active_tab();
                self.sync_watch(window, cx);
                self.focus_tree(window, cx);
                hud::log_event(&format!(
                    "file trashed path={} to={}",
                    path.display(),
                    landed.display()
                ));
                if !unsaved.is_empty() {
                    alert(
                        window,
                        cx,
                        "Du texte n'a pas pu être enregistré.",
                        Some(&unsaved.join(", ")),
                    );
                }
                cx.notify();
            }
            Err(error) => {
                hud::log_event(&format!(
                    "file trash failed path={} error={error}",
                    path.display()
                ));
                alert(
                    window,
                    cx,
                    "Impossible de mettre à la Corbeille.",
                    Some(&error.to_string()),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_real_text_change_clears_a_name_refusal() {
        let mut last = String::from("pris.md");
        let taken = Some(NameError::Taken);
        // `⌘⌫` au début du texte, ou chasse arrière sans rien devant : `Changed` rend le même texte.
        assert_eq!(error_after_change(taken, &mut last, "pris.md"), taken);
        assert_eq!(last, "pris.md");
        // Une frappe, un effacement ou un collage change le texte : le message part.
        assert_eq!(error_after_change(taken, &mut last, "pri.md"), None);
        assert_eq!(last, "pri.md");
        // Sans refus affiché, rien n'apparaît.
        assert_eq!(error_after_change(None, &mut last, "pri.md"), None);
    }

    #[test]
    fn a_refused_or_idle_drop_highlights_nothing() {
        let root = PathBuf::from("/p");
        // Un dossier sur lui-même ou sur un de ses enfants : refusé, rien n'est éclairé.
        assert_eq!(
            highlighted_target(Path::new("/p/notes"), Some(PathBuf::from("/p/notes"))),
            None
        );
        assert_eq!(
            highlighted_target(Path::new("/p/notes"), Some(PathBuf::from("/p/notes/sub"))),
            None
        );
        // Un fichier sur son propre dossier : il y est déjà, rien n'est éclairé.
        assert_eq!(
            highlighted_target(Path::new("/p/notes/a.md"), Some(PathBuf::from("/p/notes"))),
            None
        );
        // Pas de candidat (la souris a quitté la colonne) : rien.
        assert_eq!(highlighted_target(Path::new("/p/notes/a.md"), None), None);
        // Un fichier de `notes/` sur la racine : la racine s'éclaire.
        assert_eq!(
            highlighted_target(Path::new("/p/notes/a.md"), Some(root.clone())),
            Some(root)
        );
    }
}
