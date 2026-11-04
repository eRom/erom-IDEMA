//! La section « Skills » de « Personnaliser » : les skills globales de `~/.claude/skills/` (dossiers et liens qui portent
//! un `SKILL.md`), par ordre alphabétique, un interrupteur par skill et la suppression. L'état d'une skill vit dans le
//! nœud `skillOverrides` de `~/.claude/settings.json` : `"off"` l'éteint, absente ou toute autre valeur la laisse
//! active. L'interrupteur écrit `"on"` ou `"off"` sans toucher au reste du fichier. Tout est local et rapide : pas de
//! CLI, lecture et écriture sur le fil d'interface.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use gpui::{
    AnyElement, Context, FontWeight, PromptButton, PromptLevel, SharedString, Window, div,
    prelude::*,
};
use serde_json::{Map, Value};

use super::controls::{error_line, icon_button, toggle};
use super::mcp_section::notice_banner;
use super::nav::Section;
use super::plugins_section::{MENU_COLUMN, ROW_HEIGHT, TOGGLE_COLUMN};
use super::view::SettingsView;
use crate::assets::Icon;
use crate::files::ops::move_to_trash;
use crate::files::panel::home_dir;
use crate::hud;
use crate::theme::{ERROR_TEXT, SEPARATOR, TEXT_MIN, TEXT_PRIMARY, Theme, color, white};

const OVERRIDES_KEY: &str = "skillOverrides";
const OFF: &str = "off";
const ON: &str = "on";
const NO_SKILL: &str = "Aucune skill dans ~/.claude/skills.";
/// La colonne du nom ; la description prend le reste.
const NAME_COLUMN: f32 = 190.;

/// Une skill de `~/.claude/skills/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillEntry {
    /// Le nom du dossier, celui que `skillOverrides` porte en clé.
    pub name: String,
    /// La `description` du front matter de `SKILL.md`, vide si absente.
    pub description: String,
    pub enabled: bool,
    /// Vrai pour un lien symbolique : la suppression ne met à la Corbeille que le lien.
    pub linked: bool,
}

/// Le nœud `skillOverrides` de `settings`, vide s'il manque. Un fichier vide vaut un objet vide.
pub fn read_overrides(settings: &str) -> Result<Map<String, Value>, String> {
    if settings.trim().is_empty() {
        return Ok(Map::new());
    }
    let root: Value = serde_json::from_str(settings)
        .map_err(|error| format!("settings.json illisible : {error}"))?;
    match root.get(OVERRIDES_KEY) {
        None => Ok(Map::new()),
        Some(Value::Object(map)) => Ok(map.clone()),
        Some(_) => Err(format!("« {OVERRIDES_KEY} » n'est pas un objet.")),
    }
}

/// Active si et seulement si la valeur n'est pas `"off"`.
pub fn is_enabled(overrides: &Map<String, Value>, name: &str) -> bool {
    overrides.get(name).and_then(Value::as_str) != Some(OFF)
}

/// La `description` du front matter YAML de `SKILL.md` : valeur sur la ligne, guillemets retirés, ou bloc `>`/`|` replié
/// en une ligne. Vide sans front matter ni description.
pub fn frontmatter_description(text: &str) -> String {
    let mut lines = text.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return String::new();
    }
    let mut lines = lines.take_while(|line| line.trim_end() != "---").peekable();
    while let Some(line) = lines.next() {
        let Some(value) = line.strip_prefix("description:") else {
            continue;
        };
        let value = value.trim();
        if value.starts_with('>') || value.starts_with('|') {
            let mut folded = Vec::new();
            while let Some(next) = lines.peek() {
                if !next.starts_with(' ') && !next.starts_with('\t') && !next.trim().is_empty() {
                    break;
                }
                folded.push(next.trim());
                lines.next();
            }
            return folded
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
        }
        return unquote(value).to_string();
    }
    String::new()
}

fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

/// Les skills de `dir` (entrées qui portent un `SKILL.md`, lien suivi), par nom sans casse. Un dossier absent n'en a
/// aucune.
pub fn list_skills(dir: &Path, overrides: &Map<String, Value>) -> io::Result<Vec<SkillEntry>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut skills = Vec::new();
    for entry in entries {
        let entry = entry?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let manifest = entry.path().join("SKILL.md");
        if !manifest.is_file() {
            continue;
        }
        skills.push(SkillEntry {
            description: frontmatter_description(
                &fs::read_to_string(&manifest).unwrap_or_default(),
            ),
            enabled: is_enabled(overrides, &name),
            linked: entry.file_type()?.is_symlink(),
            name,
        });
    }
    skills.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(skills)
}

/// `settings` avec `skillOverrides[name]` posé à `value`, ou retiré avec `None`. L'ordre des clés est conservé
/// (`preserve_order`) et la sortie suit le format de Claude Code (deux espaces, saut de ligne final si l'original en
/// avait un, ou s'il était vide).
pub fn set_override(settings: &str, name: &str, value: Option<&str>) -> Result<String, String> {
    let mut root: Value = if settings.trim().is_empty() {
        Value::Object(Map::new())
    } else {
        serde_json::from_str(settings)
            .map_err(|error| format!("settings.json illisible : {error}"))?
    };
    let Value::Object(object) = &mut root else {
        return Err("settings.json n'est pas un objet.".to_string());
    };
    // Retirer une surcharge absente ne touche pas au fichier (ni ne le crée).
    if value.is_none()
        && !object
            .get(OVERRIDES_KEY)
            .and_then(Value::as_object)
            .is_some_and(|overrides| overrides.contains_key(name))
    {
        return Ok(settings.to_string());
    }
    let overrides = object
        .entry(OVERRIDES_KEY)
        .or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(overrides) = overrides else {
        return Err(format!("« {OVERRIDES_KEY} » n'est pas un objet."));
    };
    match value {
        Some(value) => {
            overrides.insert(name.to_string(), Value::String(value.to_string()));
        }
        None => {
            overrides.shift_remove(name);
        }
    }
    let mut text = serde_json::to_string_pretty(&root).map_err(|error| error.to_string())?;
    if settings.is_empty() || settings.ends_with('\n') {
        text.push('\n');
    }
    Ok(text)
}

/// Lit `settings` (absent : vide), applique `set_override` et réécrit le fichier s'il change, par un fichier voisin
/// renommé pour qu'une session Claude ne lise jamais un fichier à moitié écrit. Un lien est suivi ; un lien cassé est
/// refusé (le renommage le remplacerait par un fichier ordinaire). Un fichier présent mais vide est refusé aussi : c'est
/// peut-être un autre outil en pleine réécriture, et le remplacer perdrait tout son contenu.
fn write_override(settings: &Path, name: &str, value: Option<&str>) -> Result<(), String> {
    let target = match fs::canonicalize(settings) {
        Ok(target) => target,
        Err(_) if fs::symlink_metadata(settings).is_ok_and(|meta| meta.is_symlink()) => {
            return Err(format!(
                "{} : lien cassé, rien n'a été écrit.",
                settings.display()
            ));
        }
        Err(_) => settings.to_path_buf(),
    };
    let current = match fs::read_to_string(&target) {
        Ok(text) if text.trim().is_empty() => {
            return Err(format!(
                "{} est vide : rien n'a été écrit.",
                target.display()
            ));
        }
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("{} : {error}", target.display())),
    };
    let next = set_override(&current, name, value)?;
    if next == current {
        return Ok(());
    }
    let temporary = target.with_extension("json.idema-tmp");
    let written = fs::write(&temporary, &next).and_then(|()| {
        if let Ok(metadata) = fs::metadata(&target) {
            fs::set_permissions(&temporary, metadata.permissions())?;
        }
        fs::rename(&temporary, &target)
    });
    written.map_err(|error| {
        let _ = fs::remove_file(&temporary);
        format!("{} : {error}", target.display())
    })
}

/// La liste lue à l'instant : `settings.json` d'abord (sans lui, l'état des skills est inconnu), puis le dossier.
fn read_skills(dir: &Path, settings: &Path) -> Result<Vec<SkillEntry>, String> {
    let text = match fs::read_to_string(settings) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("{} : {error}", settings.display())),
    };
    let overrides = read_overrides(&text)?;
    list_skills(dir, &overrides).map_err(|error| format!("{} : {error}", dir.display()))
}

/// Le compteur de la barre latérale : skills actives sur skills présentes.
pub fn skill_counter(skills: &[SkillEntry]) -> String {
    let enabled = skills.iter().filter(|skill| skill.enabled).count();
    format!("{enabled}/{}", skills.len())
}

/// L'échec du dernier geste, sous la ligne de sa skill.
struct SkillFailure {
    name: String,
    text: String,
}

/// L'état de la section, tenu par `SettingsView`.
pub(super) struct SkillsState {
    dir: PathBuf,
    settings: PathBuf,
    skills: Result<Vec<SkillEntry>, String>,
    failure: Option<SkillFailure>,
    /// La skill dont le menu `more_horiz` est ouvert.
    menu: Option<String>,
}

impl SkillsState {
    pub(super) fn new() -> Self {
        let claude = home_dir().unwrap_or_default().join(".claude");
        Self {
            dir: claude.join("skills"),
            settings: claude.join("settings.json"),
            skills: Ok(Vec::new()),
            failure: None,
            menu: None,
        }
    }

    fn find(&self, name: &str) -> Option<&SkillEntry> {
        self.skills
            .as_ref()
            .ok()
            .and_then(|skills| skills.iter().find(|skill| skill.name == name))
    }
}

impl SettingsView {
    // ---- Lecture et gestes ------------------------------------------------------------------------------------------

    /// Relit le dossier et `settings.json` : à l'ouverture de la fenêtre et de la section, au retour au premier plan,
    /// après chaque geste.
    pub(super) fn refresh_skills(&mut self, cx: &mut Context<Self>) {
        let state = &mut self.skills;
        state.skills = read_skills(&state.dir, &state.settings);
        if let Some(open) = state.menu.clone()
            && state.find(&open).is_none()
        {
            state.menu = None;
        }
        let count = state
            .skills
            .as_ref()
            .ok()
            .map(|skills| skill_counter(skills));
        self.set_count(Section::Skills, count, cx);
        cx.notify();
    }

    /// L'interrupteur : `"off"` pour éteindre, `"on"` pour rallumer, seulement pour une skill de la dernière liste.
    fn toggle_skill(&mut self, name: &str, cx: &mut Context<Self>) {
        // Un appel `claude` de Plugins écrit le même settings.json : pas de geste pendant qu'il court.
        if self.plugins.busy.is_some() {
            return;
        }
        let Some(skill) = self.skills.find(name) else {
            return;
        };
        let value = if skill.enabled { OFF } else { ON };
        let result = write_override(&self.skills.settings, name, Some(value));
        self.finish_skill_gesture(name, result, cx);
    }

    fn finish_skill_gesture(
        &mut self,
        name: &str,
        result: Result<(), String>,
        cx: &mut Context<Self>,
    ) {
        self.skills.failure = result.err().map(|text| SkillFailure {
            name: name.to_string(),
            text,
        });
        self.refresh_skills(cx);
    }

    fn toggle_skill_menu(&mut self, name: &str, cx: &mut Context<Self>) {
        let open = self.skills.menu.as_deref() == Some(name);
        self.skills.menu = (!open).then(|| name.to_string());
        cx.notify();
    }

    /// « Supprimer » du menu : la confirmation, puis la Corbeille.
    fn confirm_skill_removal(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.skills.menu = None;
        cx.notify();
        let Some(skill) = self.skills.find(name) else {
            return;
        };
        let path = self.skills.dir.join(name);
        let detail = if skill.linked {
            let target = fs::read_link(&path)
                .map(|target| target.display().to_string())
                .unwrap_or_default();
            format!("Le lien part à la Corbeille. Sa cible reste en place : {target}")
        } else {
            format!("Le dossier {} part à la Corbeille.", path.display())
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Supprimer la skill « {name} » ?"),
            Some(&detail),
            &[
                PromptButton::new("Supprimer"),
                PromptButton::cancel("Annuler"),
            ],
            cx,
        );
        let name = name.to_string();
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(0) = answer.await {
                let _ = this.update_in(cx, |this, _, cx| this.remove_skill(&name, cx));
            }
        })
        .detach();
    }

    /// Met l'entrée à la Corbeille (le lien lui-même pour un lien), puis retire sa surcharge devenue orpheline.
    fn remove_skill(&mut self, name: &str, cx: &mut Context<Self>) {
        if self.plugins.busy.is_some() || self.skills.find(name).is_none() {
            return;
        }
        let path = self.skills.dir.join(name);
        let result = match move_to_trash(&path) {
            Ok(landed) => {
                hud::log_event(&format!(
                    "skill trashed path={} to={}",
                    path.display(),
                    landed.display()
                ));
                write_override(&self.skills.settings, name, None)
            }
            Err(error) => {
                hud::log_event(&format!(
                    "skill trash failed path={} error={error}",
                    path.display()
                ));
                Err(error.to_string())
            }
        };
        self.finish_skill_gesture(name, result, cx);
    }

    // ---- Rendu ------------------------------------------------------------------------------------------------------

    pub(super) fn render_skills_section(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let content = match self.skills.skills.clone() {
            Err(message) => div()
                .pt(theme.px(8.))
                .text_color(color(ERROR_TEXT))
                .child(SharedString::from(message))
                .into_any_element(),
            Ok(skills) => self
                .render_skill_list(&skills, theme, cx)
                .into_any_element(),
        };
        // Le menu d'une dernière ligne déborde sous la liste : la marge basse lui laisse la place de défiler.
        div()
            .id(("settings-body", Section::Skills as usize))
            .size_full()
            .overflow_y_scroll()
            .px(theme.px(20.))
            .pb(theme.px(60.))
            .child(content)
            .into_any_element()
    }

    fn render_skill_list(
        &mut self,
        skills: &[SkillEntry],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let head = div()
            .flex()
            .items_center()
            .h(theme.px(28.))
            .border_b_1()
            .border_color(white(SEPARATOR))
            .text_size(theme.text(11.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(white(TEXT_MIN))
            .child(div().w(theme.px(NAME_COLUMN)).flex_none().child("Skill"))
            .child(div().flex_1().child("Description"))
            .child(div().w(theme.px(TOGGLE_COLUMN)).flex_none().child("Actif"))
            .child(div().w(theme.px(MENU_COLUMN)).flex_none());
        let mut list = div().flex().flex_col().child(head);
        for skill in skills {
            list = list.child(self.render_skill_row(skill, theme, cx));
        }
        let mut body = div().flex().flex_col().child(notice_banner(theme));
        // L'échec d'une skill disparue entre-temps (supprimée, ou mise à la Corbeille sans sa surcharge) se lit tout de
        // même.
        if let Some(failure) = self
            .skills
            .failure
            .as_ref()
            .filter(|failure| !skills.iter().any(|skill| skill.name == failure.name))
        {
            body = body.child(error_line(failure.text.clone(), theme).mb(theme.px(6.)));
        }
        body = body.child(list);
        if skills.is_empty() {
            body = body.child(
                div()
                    .py(theme.px(12.))
                    .text_color(white(TEXT_MIN))
                    .child(NO_SKILL),
            );
        }
        body
    }

    /// Une ligne : nom (atténué éteint, `↗` pour un lien), description en tertiaire, interrupteur, menu `more_horiz`.
    fn render_skill_row(
        &mut self,
        skill: &SkillEntry,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let toggle_name = skill.name.clone();
        let menu_name = skill.name.clone();
        let remove_name = skill.name.clone();

        let switch = div()
            .id(SharedString::from(format!(
                "settings-skill-toggle-{}",
                skill.name
            )))
            .flex()
            .items_center()
            .w(theme.px(TOGGLE_COLUMN))
            .flex_none()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_skill(&toggle_name, cx)))
            .child(toggle(skill.enabled, theme));

        let mut menu_button = icon_button(
            SharedString::from(format!("settings-skill-menu-{}", skill.name)),
            Icon::MoreHoriz,
            true,
            theme,
        )
        .on_click(cx.listener(move |this, _, _, cx| this.toggle_skill_menu(&menu_name, cx)));
        if self.skills.menu.as_deref() == Some(skill.name.as_str()) {
            menu_button = menu_button.child(self.render_row_menu(
                "settings-skill-menu-remove",
                |this| this.skills.menu = None,
                move |this, window, cx| this.confirm_skill_removal(&remove_name, window, cx),
                theme,
                cx,
            ));
        }

        let name = if skill.linked {
            format!("{} ↗", skill.name)
        } else {
            skill.name.clone()
        };
        let row = div()
            .flex()
            .items_center()
            .h(theme.px(ROW_HEIGHT))
            .border_b_1()
            .border_color(white(SEPARATOR))
            .child(
                div()
                    .w(theme.px(NAME_COLUMN))
                    .flex_none()
                    .pr(theme.px(10.))
                    .truncate()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(white(if skill.enabled {
                        TEXT_PRIMARY
                    } else {
                        TEXT_MIN
                    }))
                    .child(SharedString::from(name)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .pr(theme.px(12.))
                    .truncate()
                    .text_size(theme.text(11.5))
                    .text_color(white(TEXT_MIN))
                    .child(SharedString::from(skill.description.clone())),
            )
            .child(switch)
            .child(
                div()
                    .flex()
                    .justify_end()
                    .w(theme.px(MENU_COLUMN))
                    .flex_none()
                    .child(menu_button),
            );
        let failure = self
            .skills
            .failure
            .as_ref()
            .filter(|failure| failure.name == skill.name);
        div().flex().flex_col().child(row).children(
            failure.map(|failure| error_line(failure.text.clone(), theme).mb(theme.px(6.))),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Un dossier jetable propre au test, retiré à la fin.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "idema-skills-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn skill(dir: &Path, name: &str, manifest: &str) {
        fs::create_dir_all(dir.join(name)).unwrap();
        fs::write(dir.join(name).join("SKILL.md"), manifest).unwrap();
    }

    fn overrides(json: &str) -> Map<String, Value> {
        read_overrides(json).unwrap()
    }

    #[test]
    fn only_off_disables_a_skill() {
        let map = overrides(r#"{"skillOverrides": {"a": "off", "b": "on", "c": "name-only"}}"#);
        assert!(!is_enabled(&map, "a"));
        assert!(is_enabled(&map, "b"));
        assert!(is_enabled(&map, "c"));
        assert!(is_enabled(&map, "absent"));
        assert!(read_overrides("").unwrap().is_empty());
        assert!(read_overrides("{}").unwrap().is_empty());
        assert!(read_overrides(r#"{"skillOverrides": []}"#).is_err());
        assert!(read_overrides("not json").is_err());
    }

    #[test]
    fn skills_are_listed_alphabetically_with_their_state() {
        let scratch = Scratch::new();
        let dir = scratch.0.join("skills");
        skill(
            &dir,
            "zeta",
            "---\nname: zeta\ndescription: Last one\n---\n",
        );
        skill(&dir, "Alpha", "no front matter");
        skill(&dir, "beta", "---\ndescription: \"Quoted\"\n---\n");
        fs::create_dir_all(dir.join("synced")).unwrap();
        let target = scratch.0.join("elsewhere");
        skill(
            &target,
            "linked",
            "---\ndescription: >\n  Folded\n  text\nname: linked\n---\n",
        );
        std::os::unix::fs::symlink(target.join("linked"), dir.join("gamma")).unwrap();

        let skills =
            list_skills(&dir, &overrides(r#"{"skillOverrides": {"beta": "off"}}"#)).unwrap();
        let names: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
        assert_eq!(names, ["Alpha", "beta", "gamma", "zeta"]);
        assert_eq!(skills[0].description, "");
        assert_eq!(skills[1].description, "Quoted");
        assert!(!skills[1].enabled);
        assert!(skills[2].linked && skills[2].enabled);
        assert_eq!(skills[2].description, "Folded text");
        assert_eq!(skills[3].description, "Last one");
        assert!(!skills[3].linked);
        assert_eq!(skill_counter(&skills), "3/4");
        assert!(
            list_skills(&scratch.0.join("missing"), &Map::new())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn set_override_touches_only_its_key_and_keeps_order() {
        let original = "{\n  \"z\": 1,\n  \"skillOverrides\": {\n    \"b\": \"off\"\n  },\n  \"a\": [\n    \"x\"\n  ]\n}\n";
        let off = set_override(original, "c", Some("off")).unwrap();
        assert_eq!(
            off,
            "{\n  \"z\": 1,\n  \"skillOverrides\": {\n    \"b\": \"off\",\n    \"c\": \"off\"\n  },\n  \"a\": [\n    \"x\"\n  ]\n}\n"
        );
        let on = set_override(&off, "b", Some("on")).unwrap();
        assert!(on.contains("\"b\": \"on\",\n    \"c\": \"off\""));
        let removed = set_override(&on, "b", None).unwrap();
        assert!(!removed.contains("\"b\""));
        assert_eq!(set_override(original, "b", Some("off")).unwrap(), original);
        assert_eq!(
            set_override("", "a", Some("off")).unwrap(),
            "{\n  \"skillOverrides\": {\n    \"a\": \"off\"\n  }\n}\n"
        );
        assert_eq!(set_override(original, "absent", None).unwrap(), original);
        assert_eq!(
            set_override("{\"z\": 1}", "absent", None).unwrap(),
            "{\"z\": 1}"
        );
        assert!(set_override("[]", "a", Some("off")).is_err());
        assert!(set_override("{\"skillOverrides\": 1}", "a", Some("off")).is_err());
    }

    #[test]
    fn write_override_rewrites_the_file_in_place() {
        let scratch = Scratch::new();
        let settings = scratch.0.join("settings.json");
        write_override(&settings, "a", Some("off")).unwrap();
        assert!(!is_enabled(
            &overrides(&fs::read_to_string(&settings).unwrap()),
            "a"
        ));
        write_override(&settings, "a", Some("on")).unwrap();
        assert!(is_enabled(
            &overrides(&fs::read_to_string(&settings).unwrap()),
            "a"
        ));
        assert!(!scratch.0.join("settings.json.idema-tmp").exists());
        fs::write(&settings, "").unwrap();
        assert!(write_override(&settings, "a", Some("off")).is_err());
        assert_eq!(fs::read_to_string(&settings).unwrap(), "");
        let dangling = scratch.0.join("dangling.json");
        std::os::unix::fs::symlink(scratch.0.join("nowhere.json"), &dangling).unwrap();
        assert!(write_override(&dangling, "a", Some("off")).is_err());
        assert!(fs::symlink_metadata(&dangling).unwrap().is_symlink());
        let absent = scratch.0.join("absent.json");
        write_override(&absent, "a", None).unwrap();
        assert!(!absent.exists());
        fs::write(&settings, "{broken").unwrap();
        assert!(write_override(&settings, "a", Some("off")).is_err());
        assert_eq!(fs::read_to_string(&settings).unwrap(), "{broken");
    }
}
