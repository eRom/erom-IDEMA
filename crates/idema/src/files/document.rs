//! Les règles d'un document texte du volet Fichiers (spec P2 §4.2, §4.5, §4.6) : ce qui s'ouvre dans l'éditeur et ce
//! qui part vers l'app par défaut, la langue de coloration, la fin de ligne et l'indentation à conserver, et la
//! sauvegarde atomique. Aucune dépendance à GPUI : tout se teste sur le disque.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use futures::channel::oneshot::{self, Receiver};

use idema_tmux::env::server_env;

use crate::bootstrap::apple_locale;
use crate::files::tabs::{Fingerprint, fingerprint};
use crate::hud;

/// Au-delà, un fichier s'ouvre dans l'app par défaut plutôt que dans l'éditeur (spec §4.2, seuil à revoir à l'usage).
pub const MAX_EDITABLE_BYTES: u64 = 5 * 1024 * 1024;

/// Le nombre d'octets du début d'un fichier où un `\0` le déclare binaire.
const BINARY_PROBE_BYTES: usize = 8 * 1024;

/// Indentation d'un fichier qui n'en a pas, ou qu'on ne sait pas lire (spec §4.5 : la norme du YAML).
const DEFAULT_INDENT_WIDTH: u8 = 2;

/// Les pas d'indentation en espaces qu'on sait lire ; hors de cet intervalle, la valeur par défaut.
const MIN_INDENT_WIDTH: usize = 2;
const MAX_INDENT_WIDTH: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    Crlf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Indent {
    Spaces(u8),
    Tabs,
}

/// Un texte lu du disque. `text` est toujours en `\n` : la fin de ligne d'origine se remet à la sauvegarde.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedText {
    pub text: String,
    pub line_ending: LineEnding,
    pub indent: Indent,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Opened {
    Text(LoadedText),
    /// Trop gros, binaire (un `\0` dans les 8 premiers Ko) ou non UTF-8 : à confier à l'app par défaut.
    External,
}

/// Lit un fichier et décide s'il s'ouvre dans l'éditeur. `Err` seulement quand le disque refuse la lecture.
pub fn read_document(path: &Path) -> io::Result<Opened> {
    let meta = fs::metadata(path)?;
    // Un dossier, un tube ou un périphérique ne s'édite pas : l'app par défaut décidera.
    if !meta.is_file() || meta.len() > MAX_EDITABLE_BYTES {
        return Ok(Opened::External);
    }
    let bytes = fs::read(path)?;
    let probe = &bytes[..bytes.len().min(BINARY_PROBE_BYTES)];
    if probe.contains(&0) {
        return Ok(Opened::External);
    }
    let Ok(raw) = String::from_utf8(bytes) else {
        return Ok(Opened::External);
    };
    let line_ending = detect_line_ending(&raw);
    let text = match line_ending {
        LineEnding::Lf => raw,
        LineEnding::Crlf => raw.replace("\r\n", "\n"),
    };
    let indent = detect_indent(&text);
    Ok(Opened::Text(LoadedText {
        text,
        line_ending,
        indent,
    }))
}

/// `Crlf` quand les fins de ligne `\r\n` l'emportent sur les `\n` nus ; un fichier sans fin de ligne reste `Lf`.
pub(crate) fn detect_line_ending(raw: &str) -> LineEnding {
    let crlf = raw.matches("\r\n").count();
    let lf_only = raw.matches('\n').count() - crlf;
    if crlf > lf_only {
        LineEnding::Crlf
    } else {
        LineEnding::Lf
    }
}

/// Les tabulations quand elles ouvrent la majorité des lignes indentées ; sinon des espaces, de la largeur du pas
/// d'indentation dominant : parmi les lignes qui se suivent, le saut vers le haut le plus fréquent (à égalité, le plus
/// petit). Seuls les sauts de 2 à 8 comptent, ce qui écarte l'alignement d'un commentaire doc (`     * suite` à 5
/// espaces sous un `    /**` à 4 : un saut de 1) ; une ligne à un seul espace de tête est ignorée. Sans saut lisible,
/// c'est la valeur par défaut.
fn detect_indent(text: &str) -> Indent {
    let mut tabs = 0usize;
    let mut spaces = 0usize;
    let mut steps = [0usize; MAX_INDENT_WIDTH + 1];
    let mut previous = 0usize;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with('\t') {
            tabs += 1;
            previous = 0;
            continue;
        }
        let depth = line.len() - line.trim_start_matches(' ').len();
        if depth == 1 {
            continue;
        }
        if depth >= 2 {
            spaces += 1;
        }
        if let Some(step) = depth.checked_sub(previous)
            && (MIN_INDENT_WIDTH..=MAX_INDENT_WIDTH).contains(&step)
        {
            steps[step] += 1;
        }
        previous = depth;
    }
    if tabs > spaces {
        return Indent::Tabs;
    }
    let mut dominant = 0usize;
    for step in MIN_INDENT_WIDTH..=MAX_INDENT_WIDTH {
        if steps[step] > steps[dominant] {
            dominant = step;
        }
    }
    match u8::try_from(dominant) {
        Ok(width) if width >= 2 => Indent::Spaces(width),
        _ => Indent::Spaces(DEFAULT_INDENT_WIDTH),
    }
}

/// Le langage de coloration de gpui-component pour ce fichier, `None` pour du texte brut. XML emprunte la grammaire
/// HTML et `.env` celle de bash (spec §4.5) : gpui-component n'a ni l'une ni l'autre.
pub fn language_for(path: &Path) -> Option<&'static str> {
    let name = path.file_name()?.to_string_lossy().to_lowercase();
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase());
    if name == ".env" || name.starts_with(".env.") || extension.as_deref() == Some("env") {
        return Some("bash");
    }
    match extension.as_deref()? {
        "md" | "markdown" => Some("markdown"),
        "yml" | "yaml" => Some("yaml"),
        "json" => Some("json"),
        "toml" => Some("toml"),
        "html" | "htm" | "xml" => Some("html"),
        "rs" => Some("rust"),
        "ts" | "mts" | "cts" => Some("typescript"),
        "tsx" => Some("tsx"),
        "js" | "mjs" | "cjs" | "jsx" => Some("javascript"),
        "swift" => Some("swift"),
        "py" | "pyi" => Some("python"),
        // SCSS emprunte la grammaire CSS, comme le fait gpui-component.
        "css" | "scss" => Some("css"),
        _ => None,
    }
}

/// Écrit `text` (en `\n`) dans `path`, avec la fin de ligne demandée, sans jamais laisser un fichier à moitié écrit :
/// fichier temporaire `.<nom>.idema-tmp` dans le même dossier, `fsync`, permissions du fichier d'origine, puis
/// `rename`. Un lien symbolique se suit : c'est sa cible qui est remplacée, pas le lien. Un fichier existant que
/// l'utilisateur ne peut pas écrire (0444) est refusé (`PermissionDenied`) : le `rename` passerait par-dessus.
#[cfg(test)]
pub fn save_atomically(path: &Path, text: &str, line_ending: LineEnding) -> io::Result<()> {
    save_checked(path, text, line_ending, None).map(|_| ())
}

/// Ce qu'une sauvegarde gardée a fait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Saved {
    Written,
    /// Le fichier a changé entre la vérification de l'appelant et le `rename` : rien n'a été écrit, le temporaire est
    /// jeté, et l'appelant doit passer par le rechargement.
    DiskChanged,
}

/// `save_atomically`, avec une garde : `expected` est l'empreinte que le fichier avait quand l'appelant a vérifié qu'il
/// pouvait l'écrire. Elle est relue juste avant le `rename` ; si elle a changé (un agent a écrit entre-temps), le
/// temporaire est jeté et rien n'est remplacé.
pub fn save_unless_changed(
    path: &Path,
    text: &str,
    line_ending: LineEnding,
    expected: Option<Fingerprint>,
) -> io::Result<Saved> {
    save_checked(path, text, line_ending, Some(expected))
}

fn save_checked(
    path: &Path,
    text: &str,
    line_ending: LineEnding,
    guard: Option<Option<Fingerprint>>,
) -> io::Result<Saved> {
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    refuse_read_only(&target)?;
    let mut temp_name = std::ffi::OsString::from(".");
    temp_name.push(name);
    temp_name.push(".idema-tmp");
    let temp = target.with_file_name(temp_name);

    let written = write_temp(&temp, &target, text, line_ending);
    let outcome = written.and_then(|()| {
        // L'empreinte relue au dernier moment : une métadonnée illisible n'est pas un changement.
        if let Some(expected) = guard
            && let Ok(now) = fingerprint(&target)
            && now != expected
        {
            return Ok(Saved::DiskChanged);
        }
        fs::rename(&temp, &target).map(|()| Saved::Written)
    });
    if !matches!(outcome, Ok(Saved::Written)) {
        // Le temporaire est à nous : on ne laisse pas de reste derrière un échec ou un renoncement.
        let _ = fs::remove_file(&temp);
    }
    outcome
}

/// Un fichier existant que l'utilisateur ne peut pas ouvrir en écriture n'est pas réécrit : le `rename` d'un
/// temporaire ne s'en soucierait pas, la sauvegarde atomique contournerait le refus. L'ouverture ne tronque rien.
pub(crate) fn refuse_read_only(target: &Path) -> io::Result<()> {
    match OpenOptions::new().write(true).open(target) {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Err(error),
        _ => Ok(()),
    }
}

fn write_temp(temp: &Path, target: &Path, text: &str, line_ending: LineEnding) -> io::Result<()> {
    let mut file: File = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(temp)?;
    // Les permissions avant le contenu : le texte n'est jamais lisible plus largement que l'original.
    if let Ok(meta) = fs::metadata(target) {
        file.set_permissions(meta.permissions())?;
    }
    match line_ending {
        LineEnding::Lf => file.write_all(text.as_bytes())?,
        LineEnding::Crlf => file.write_all(text.replace('\n', "\r\n").as_bytes())?,
    }
    file.sync_all()
}

/// Ce que l'app fait d'un fichier qui ne s'ouvre pas dans l'éditeur.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalAction {
    /// L'app par défaut (`open <chemin>`) : une image, un PDF, un document.
    Open,
    /// Le Finder (`open -R <chemin>`) : `open` exécuterait un binaire ou extrairait une archive dans le projet.
    Reveal,
}

/// Les archives que l'app par défaut extrairait à côté du fichier.
const ARCHIVE_EXTENSIONS: [&str; 17] = [
    "zip", "tar", "gz", "tgz", "bz2", "tbz", "tbz2", "xz", "txz", "7z", "rar", "dmg", "pkg", "jar",
    "cpgz", "xip", "zst",
];

/// Les débuts de fichier d'un exécutable : Mach-O (32 et 64 bits, les deux ordres d'octets, binaire universel) et
/// le `#!` d'un script.
const MACH_O_MAGICS: [[u8; 4]; 6] = [
    [0xfe, 0xed, 0xfa, 0xce],
    [0xfe, 0xed, 0xfa, 0xcf],
    [0xce, 0xfa, 0xed, 0xfe],
    [0xcf, 0xfa, 0xed, 0xfe],
    [0xca, 0xfe, 0xba, 0xbe],
    [0xbe, 0xba, 0xfe, 0xca],
];

/// Décide si un fichier non texte part à l'app par défaut ou seulement au Finder. Un exécutable (bit x sur un fichier,
/// en-tête Mach-O ou `#!`), un dossier (paquet `.app` compris), une archive ne sont jamais passés à `open <chemin>` : `open` les lancerait
/// ou les extrairait. Un fichier qu'on ne peut pas examiner est révélé, par prudence.
pub fn external_action(path: &Path) -> ExternalAction {
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase());
    if extension
        .as_deref()
        .is_some_and(|extension| ARCHIVE_EXTENSIONS.contains(&extension))
    {
        return ExternalAction::Reveal;
    }
    let Ok(meta) = fs::metadata(path) else {
        return ExternalAction::Reveal;
    };
    // Un dossier visé comme un fichier n'est jamais ouvert : s'il est un paquet (`.app`, `.prefPane`, `.bundle`,
    // `.plugin`, `.framework`…), `open` le lancerait ; le Finder le montre sans rien exécuter.
    if !meta.is_file() || meta.permissions().mode() & 0o111 != 0 || has_executable_header(path) {
        return ExternalAction::Reveal;
    }
    ExternalAction::Open
}

fn has_executable_header(path: &Path) -> bool {
    let mut head = [0u8; 4];
    let read = File::open(path).and_then(|mut file| file.read(&mut head));
    match read {
        Ok(count) => {
            head[..count].starts_with(b"#!") || (count == 4 && MACH_O_MAGICS.contains(&head))
        }
        Err(_) => true,
    }
}

/// Confie le fichier à l'app par défaut de macOS (`open <chemin>`). Le lancement et la récolte du processus se font
/// hors du fil d'interface (pas de zombie), avec l'environnement d'IdemA filtré comme celui du serveur tmux ; le
/// résultat est au journal et revient aussi par le canal : `Err` porte le message brut (échec du lancement ou statut
/// non nul de `open`), pour la bulle d'erreur de l'appelant (spec §5). Un canal dont on a lâché le récepteur est sans
/// effet.
pub fn open_with_default_app(path: &Path) -> Receiver<Result<(), String>> {
    run_open(path, false)
}

/// Révèle le fichier dans le Finder (`open -R <chemin>`), sans rien lancer. Même canal et même journal que
/// `open_with_default_app` (« file revealed »).
pub fn reveal_in_finder(path: &Path) -> Receiver<Result<(), String>> {
    run_open(path, true)
}

fn run_open(path: &Path, reveal: bool) -> Receiver<Result<(), String>> {
    let (sender, receiver) = oneshot::channel();
    // Partagé avec le fil : si le fil ne démarre pas, l'émetteur est resté ici pour dire pourquoi.
    let sender = Arc::new(Mutex::new(Some(sender)));
    let path = path.to_path_buf();
    let spawned = std::thread::Builder::new()
        .name("idema-open".into())
        .spawn({
            let sender = Arc::clone(&sender);
            let path = path.clone();
            move || {
                // La locale se lit par `defaults` : un process, donc hors du fil d'interface.
                let env = server_env(std::env::vars(), apple_locale().as_deref());
                let mut command = Command::new("/usr/bin/open");
                if reveal {
                    command.arg("-R");
                }
                let child = command
                    .arg(&path)
                    .env_clear()
                    .envs(env)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn();
                let outcome = match child.and_then(|mut child| child.wait()) {
                    Ok(status) if status.success() => Ok(()),
                    Ok(status) => Err(status.to_string()),
                    Err(error) => Err(error.to_string()),
                };
                report(&sender, &path, reveal, outcome);
            }
        });
    if let Err(error) = spawned {
        report(&sender, &path, reveal, Err(error.to_string()));
    }
    receiver
}

/// Journal et canal : un seul envoi, quel que soit celui qui arrive le premier.
fn report(
    sender: &Mutex<Option<oneshot::Sender<Result<(), String>>>>,
    path: &Path,
    reveal: bool,
    outcome: Result<(), String>,
) {
    let (done, failed) = if reveal {
        ("file revealed", "file reveal failed")
    } else {
        ("file external", "file external failed")
    };
    match &outcome {
        Ok(()) => hud::log_event(&format!("{done} path={}", path.display())),
        Err(message) => hud::log_event(&format!("{failed} path={}: {message}", path.display())),
    }
    let taken = sender.lock().ok().and_then(|mut slot| slot.take());
    if let Some(sender) = taken {
        let _ = sender.send(outcome);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    /// Un dossier neuf, propre à ce test.
    fn folder(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("idema-document-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn loaded(dir: &Path, name: &str, content: &[u8]) -> LoadedText {
        let path = dir.join(name);
        fs::write(&path, content).unwrap();
        match read_document(&path).unwrap() {
            Opened::Text(loaded) => loaded,
            Opened::External => panic!("{name} should open as text"),
        }
    }

    #[test]
    fn language_follows_the_extension_and_the_dotenv_names() {
        let language = |name: &str| language_for(Path::new(name));
        assert_eq!(language("docs/a.md"), Some("markdown"));
        assert_eq!(language("x.yml"), Some("yaml"));
        assert_eq!(language("docker-compose.yaml"), Some("yaml"));
        assert_eq!(language("settings.json"), Some("json"));
        assert_eq!(language("Cargo.toml"), Some("toml"));
        assert_eq!(language("index.html"), Some("html"));
        // XML emprunte la grammaire HTML (spec §4.5).
        assert_eq!(language("pom.xml"), Some("html"));
        assert_eq!(language(".env"), Some("bash"));
        assert_eq!(language(".env.local"), Some("bash"));
        assert_eq!(language("prod.env"), Some("bash"));
        assert_eq!(language("src/main.rs"), Some("rust"));
        assert_eq!(language("index.ts"), Some("typescript"));
        assert_eq!(language("vite.config.mts"), Some("typescript"));
        assert_eq!(language("App.tsx"), Some("tsx"));
        assert_eq!(language("app.js"), Some("javascript"));
        assert_eq!(language("Button.jsx"), Some("javascript"));
        assert_eq!(language("eslint.config.cjs"), Some("javascript"));
        assert_eq!(language("App.swift"), Some("swift"));
        assert_eq!(language("main.py"), Some("python"));
        assert_eq!(language("styles.css"), Some("css"));
        assert_eq!(language("theme.scss"), Some("css"));
        assert_eq!(language("MAIN.RS"), Some("rust"));
        assert_eq!(language("notes.txt"), None);
        assert_eq!(language("Makefile"), None);
    }

    #[test]
    fn every_language_named_has_a_grammar_that_colours() {
        use gpui_component::highlighter::{LanguageRegistry, SyntaxHighlighter};

        // Un nom sans grammaire activée dans Cargo.toml, ou dont la requête ne compile pas, retomberait en texte brut
        // sans un mot ; une requête vide (le Swift du kit) ne colorerait rien.
        crate::files::grammars::register_code_grammars();
        let registry = LanguageRegistry::singleton();
        for name in [
            "a.md", "a.yml", "a.json", "a.toml", "a.html", ".env", "a.rs", "a.ts", "a.tsx", "a.js",
            "a.swift", "a.py", "a.css",
        ] {
            let language = language_for(Path::new(name)).unwrap();
            let grammar = registry.language(language);
            assert!(
                grammar.is_some_and(|grammar| !grammar.highlights.is_empty()),
                "{name} → {language}: no highlights query"
            );
            assert_eq!(
                SyntaxHighlighter::new(language).language().as_ref(),
                language,
                "{name}: fell back to plain text"
            );
        }
    }

    #[test]
    fn crlf_round_trips_through_save() {
        let dir = folder("crlf");
        let content = b"a\r\nb\r\n";
        let document = loaded(&dir, "crlf.txt", content);
        assert_eq!(document.text, "a\nb\n");
        assert_eq!(document.line_ending, LineEnding::Crlf);

        let path = dir.join("crlf.txt");
        save_atomically(&path, &document.text, document.line_ending).unwrap();
        assert_eq!(fs::read(&path).unwrap(), content);

        // Une ligne ajoutée : chaque ligne finit en `\r\n`, aucun `\n` nu.
        let longer = format!("{}c\n", document.text);
        save_atomically(&path, &longer, document.line_ending).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"a\r\nb\r\nc\r\n");

        // Un fichier en `\n` reste en `\n`.
        let lf = loaded(&dir, "lf.txt", b"a\nb\n");
        assert_eq!(lf.line_ending, LineEnding::Lf);
        let lf_path = dir.join("lf.txt");
        save_atomically(&lf_path, &lf.text, lf.line_ending).unwrap();
        assert_eq!(fs::read(&lf_path).unwrap(), b"a\nb\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tab_indented_files_keep_tabs() {
        let dir = folder("indent");
        let tabs = loaded(
            &dir,
            "main.go",
            b"func main() {\n\tif x {\n\t\ty()\n\t}\n}\n",
        );
        assert_eq!(tabs.indent, Indent::Tabs);

        let yaml = loaded(&dir, "a.yml", b"root:\n  child:\n    leaf: 1\n  other: 2\n");
        assert_eq!(yaml.indent, Indent::Spaces(2));

        let four = loaded(&dir, "b.py", b"def f():\n    if x:\n        return 1\n");
        assert_eq!(four.indent, Indent::Spaces(4));

        // Rien d'indenté : la norme du YAML.
        let flat = loaded(&dir, "flat.txt", b"one\ntwo\n");
        assert_eq!(flat.indent, Indent::Spaces(2));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_one_space_continuation_line_keeps_four_space_indent() {
        let dir = folder("one-space");
        let java = loaded(
            &dir,
            "A.java",
            b"/**\n * Doc.\n */\nclass A {\n    void f() {\n        g();\n    }\n}\n",
        );
        assert_eq!(java.indent, Indent::Spaces(4));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_doc_comment_block_keeps_four_space_indent() {
        let dir = folder("doc-block");
        let java = loaded(
            &dir,
            "B.java",
            b"class B {\n    /**\n     * Doc.\n     * More.\n     */\n    void f() {\n        g();\n    }\n}\n",
        );
        assert_eq!(java.indent, Indent::Spaces(4));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_indent_width_falls_back_to_the_default() {
        let dir = folder("wide-indent");
        // 10 espaces partout : au-delà de 8, ce n'est pas une indentation qu'on sait lire.
        let wide = loaded(&dir, "wide.txt", b"a:\n          b\n          c\n");
        assert_eq!(wide.indent, Indent::Spaces(2));
        // 3 espaces : une largeur plausible, elle est conservée.
        let three = loaded(&dir, "three.md", b"- a\n   - b\n      - c\n");
        assert_eq!(three.indent, Indent::Spaces(3));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_directory_is_external() {
        let dir = folder("directory");
        assert_eq!(read_document(&dir).unwrap(), Opened::External);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_open_reports_the_failure_on_the_channel() {
        let dir = folder("open-missing");
        let outcome = futures::executor::block_on(open_with_default_app(&dir.join("absent.png")));
        // `open` refuse un fichier absent sans lancer aucune app.
        assert!(matches!(outcome, Ok(Err(message)) if !message.is_empty()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn binary_large_and_invalid_utf8_files_are_external() {
        let dir = folder("external");
        let read = |name: &str, content: &[u8]| {
            let path = dir.join(name);
            fs::write(&path, content).unwrap();
            read_document(&path).unwrap()
        };

        // Un PNG minimal : la signature porte un `\0` dans les premiers octets.
        let png = [
            0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0x0d, b'I', b'H', b'D', b'R',
        ];
        assert_eq!(read("a.png", &png), Opened::External);

        let big = vec![b'a'; usize::try_from(MAX_EDITABLE_BYTES).unwrap() + 1];
        assert_eq!(read("big.txt", &big), Opened::External);
        let limit = vec![b'a'; usize::try_from(MAX_EDITABLE_BYTES).unwrap()];
        assert!(matches!(read("limit.txt", &limit), Opened::Text(_)));

        assert_eq!(read("latin.txt", &[0xff, 0xfe, b'a']), Opened::External);

        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/files/employees.yml");
        if fixture.is_file() {
            assert!(matches!(read_document(&fixture).unwrap(), Opened::Text(_)));
        } else {
            eprintln!("fixtures absentes : tests/files, employees.yml non vérifié");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_is_atomic_and_keeps_permissions() {
        let dir = folder("atomic");
        let path = dir.join("secret.env");
        fs::write(&path, "OLD=1\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

        save_atomically(&path, "NEW=2\n", LineEnding::Lf).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "NEW=2\n");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
        let names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["secret.env".to_owned()]);
        let _ = fs::remove_dir_all(&dir);
    }

    fn write_with_mode(dir: &Path, name: &str, content: &[u8], mode: u32) -> std::path::PathBuf {
        let path = dir.join(name);
        fs::write(&path, content).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[test]
    fn executables_and_archives_are_revealed_never_opened() {
        let dir = folder("external-action");
        let png = [
            0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0x0d, b'I', b'H', b'D', b'R',
        ];
        // Un binaire avec le bit x, un Mach-O sans bit x, un script `#!` non texte : jamais lancés.
        let tool = write_with_mode(&dir, "tool", &[0, 1, 2, 3, 4], 0o755);
        let mach_o = write_with_mode(&dir, "lib.bin", &[0xcf, 0xfa, 0xed, 0xfe, 7, 7], 0o644);
        let script = write_with_mode(&dir, "run.dat", b"#!/bin/sh\n\xff\xfe", 0o644);
        // Une archive, quelle que soit la casse, n'est pas extraite à côté du fichier.
        for name in [
            "a.zip", "b.TAR", "c.gz", "d.tgz", "e.bz2", "f.xz", "g.7z", "h.rar", "i.dmg", "j.pkg",
            "k.jar", "l.tbz", "m.tbz2", "n.txz", "o.cpgz", "p.xip", "q.zst",
        ] {
            let archive = write_with_mode(&dir, name, &[0x50, 0x4b, 3, 4], 0o644);
            assert_eq!(external_action(&archive), ExternalAction::Reveal, "{name}");
        }
        for path in [&tool, &mach_o, &script] {
            assert_eq!(external_action(path), ExternalAction::Reveal, "{path:?}");
        }
        // Tout dossier visé comme un fichier, paquet ou non, est révélé : `open` lancerait un paquet.
        for name in [
            "Tool.app",
            "Pane.prefPane",
            "Kit.bundle",
            "Ext.plugin",
            "Lib.framework",
            "plain",
        ] {
            let folder = dir.join(name);
            fs::create_dir(&folder).unwrap();
            assert_eq!(external_action(&folder), ExternalAction::Reveal, "{name}");
        }
        // Un fichier qu'on ne peut pas examiner est révélé, pas ouvert.
        assert_eq!(
            external_action(&dir.join("absent.png")),
            ExternalAction::Reveal
        );
        // Une image ou un PDF ordinaire gardent l'ouverture normale ; le dossier, révélé.
        let image = write_with_mode(&dir, "a.png", &png, 0o644);
        let pdf = write_with_mode(&dir, "a.pdf", b"%PDF-1.7\n\0\0", 0o644);
        assert_eq!(external_action(&image), ExternalAction::Open);
        assert_eq!(external_action(&pdf), ExternalAction::Open);
        assert_eq!(external_action(&dir), ExternalAction::Reveal);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_read_only_file_in_a_writable_folder_is_not_rewritten() {
        let dir = folder("read-only");
        let path = write_with_mode(&dir, "locked.txt", b"keep\n", 0o444);
        let error = save_atomically(&path, "overwritten\n", LineEnding::Lf).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(fs::read_to_string(&path).unwrap(), "keep\n");
        // Aucun temporaire ne reste, et le refus est le même par la voie gardée.
        let guarded =
            save_unless_changed(&path, "x\n", LineEnding::Lf, fingerprint(&path).unwrap());
        assert_eq!(guarded.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_guarded_save_over_a_file_that_changed_since_the_check_writes_nothing() {
        let dir = folder("guarded");
        let path = dir.join("a.txt");
        fs::write(&path, "one").unwrap();
        let known = fingerprint(&path).unwrap();
        // Rien n'a bougé : la garde laisse passer.
        assert_eq!(
            save_unless_changed(&path, "mine\n", LineEnding::Lf, known).unwrap(),
            Saved::Written
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "mine\n");

        // Un agent écrit après la vérification : le temporaire est jeté, le fichier de l'agent reste.
        let stale = fingerprint(&path).unwrap();
        fs::write(&path, "the agent wrote something longer").unwrap();
        assert_eq!(
            save_unless_changed(&path, "late\n", LineEnding::Lf, stale).unwrap(),
            Saved::DiskChanged
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "the agent wrote something longer"
        );
        // Un fichier disparu depuis la vérification n'est pas recréé non plus.
        let present = fingerprint(&path).unwrap();
        fs::remove_file(&path).unwrap();
        assert_eq!(
            save_unless_changed(&path, "late\n", LineEnding::Lf, present).unwrap(),
            Saved::DiskChanged
        );
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        let _ = fs::remove_dir_all(&dir);
    }
}
