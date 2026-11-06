//! Le texte d'une notification : le titre, les dernières lignes utiles de l'écran du pane, et la charge utile JSON que
//! le service worker lira. Tout est pur : l'écran capturé arrive en paramètre.

use std::fmt::Write as _;

use super::episodes::EpisodeKind;

/// La charge utile JSON tient en 3 Ko au plus (Apple accepte 4 Ko, l'enregistrement chiffré ajoute son en-tête).
pub const PAYLOAD_MAX_BYTES: usize = 3072;
/// Une ligne du corps est coupée à ce nombre de caractères, suivis de « … ».
pub const LINE_MAX_CHARS: usize = 110;
/// Le nom d'un agent, d'une session ou d'un projet est coupé à ce nombre de caractères dans le titre.
const NAME_MAX_CHARS: usize = 60;
/// Les traits verticaux d'un cadre de TUI (`│`, `┃`, `║`) : ils ne font pas partie du texte de la ligne.
const FRAME_VERTICALS: [char; 3] = ['│', '┃', '║'];

/// Retire les séquences d'échappement (couleurs SGR, déplacements, titres de fenêtre, liens) et les caractères de
/// contrôle, sauf la fin de ligne et la tabulation. Une séquence tronquée ou inconnue disparaît sans rien emporter
/// d'autre qu'elle ; aucun octet n'est jamais pris pour du balisage.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    skip_csi(&mut chars);
                }
                // OSC (titre, lien), puis DCS, SOS, PM et APC : jusqu'à BEL ou ST.
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    chars.next();
                    skip_string(&mut chars);
                }
                // Échappement d'un octet, avec ses éventuels octets intermédiaires (`ESC ( B`).
                Some(next) if ('\u{20}'..='\u{2f}').contains(&next) => {
                    while chars
                        .next_if(|c| ('\u{20}'..='\u{2f}').contains(c))
                        .is_some()
                    {}
                    chars.next_if(|c| ('\u{30}'..='\u{7e}').contains(c));
                }
                Some(next) if ('\u{30}'..='\u{7e}').contains(&next) => {
                    chars.next();
                }
                _ => {}
            },
            // CSI sur 8 bits (U+009B) : traité comme `ESC [`.
            '\u{9b}' => skip_csi(&mut chars),
            '\n' | '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Consomme le reste d'une séquence CSI : paramètres et intermédiaires, puis l'octet final. Un caractère hors de ces
/// plages (une fin de ligne, du texte non ASCII) arrête la séquence sans être consommé.
fn skip_csi(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(next) = chars.peek().copied() {
        if ('\u{20}'..='\u{3f}').contains(&next) {
            chars.next();
        } else {
            if ('\u{40}'..='\u{7e}').contains(&next) {
                chars.next();
            }
            return;
        }
    }
}

/// Consomme une chaîne de commande (OSC et apparentées) jusqu'à BEL ou ST (`ESC \`). Une fin de ligne l'arrête sans être
/// consommée : une chaîne jamais terminée ne doit pas avaler les lignes qui suivent.
fn skip_string(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(next) = chars.peek().copied() {
        match next {
            '\n' => return,
            '\u{7}' => {
                chars.next();
                return;
            }
            '\u{1b}' => {
                chars.next();
                chars.next_if_eq(&'\\');
                return;
            }
            _ => {
                chars.next();
            }
        }
    }
}

/// Une ligne de l'écran telle qu'on la montre : blancs et traits de cadre retirés aux deux bouts, `None` si elle n'a ni
/// lettre ni chiffre, coupée à 110 caractères suivis de « … ».
fn clean_line(line: &str) -> Option<String> {
    let text = line.trim_matches(|c: char| c.is_whitespace() || FRAME_VERTICALS.contains(&c));
    if !text.chars().any(char::is_alphanumeric) {
        return None;
    }
    Some(cut(text, LINE_MAX_CHARS))
}

/// Au plus `max` caractères, suivis de « … » quand il a fallu couper.
fn cut(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

/// Un trait horizontal de cadre : rien que des caractères de dessin de boîte (et des blancs), dont au moins un trait
/// horizontal. `────────`, `╭──────╮`, `╰──────╯`, `└──┴──┘`, mais ni `│  │` ni `-----`.
fn is_frame_line(line: &str) -> bool {
    const HORIZONTALS: [char; 9] = ['─', '━', '═', '┄', '┅', '┈', '┉', '╌', '╍'];
    let line = line.trim();
    !line.is_empty()
        && line
            .chars()
            .all(|c| c.is_whitespace() || ('\u{2500}'..='\u{257f}').contains(&c))
        && line.chars().any(|c| HORIZONTALS.contains(&c))
}

/// Le haut d'une boîte (`╭──╮`) : ce qui est en dessous en fait partie, ce n'est pas un pied de page.
fn is_box_top(line: &str) -> bool {
    line.contains(['╭', '┌', '╔', '┏'])
}

/// Le bas d'une boîte (`╰──╯`).
fn is_box_bottom(line: &str) -> bool {
    line.contains(['╯', '┘', '╝', '┛'])
}

/// Une invite vide : `>`, `❯` ou `›` sans rien après (le trait vertical d'un cadre mis à part).
fn is_bare_prompt(line: &str) -> bool {
    let line = line.trim_matches(|c: char| c.is_whitespace() || FRAME_VERTICALS.contains(&c));
    matches!(line, ">" | "❯" | "›")
}

/// Le bloc entre deux traits est-il la boîte de saisie de l'agent : au moins une invite vide, rien d'autre que du blanc ?
fn is_input_box(block: &[&str]) -> bool {
    block.iter().any(|line| is_bare_prompt(line))
        && block
            .iter()
            .all(|line| is_bare_prompt(line) || clean_line(line).is_none())
}

/// Combien de lignes de texte au plus peuvent se trouver entre le bas d'une boîte et la boîte de saisie (« Esc to
/// cancel ») sans que ce soit autre chose qu'un rappel de touches.
const HINT_LINES_MAX: usize = 2;

/// Où finit le corps de l'écran (les lignes au-dessus ne sont pas du pied de page). Tout ce qui est sous le dernier trait
/// horizontal est le pied de page de l'agent (statut, quotas, mode), tant qu'il reste du texte au-dessus ; la boîte de
/// saisie vide qui le précède (trait, invite vide, trait) l'est aussi, et, dans ce cas, le rappel de touches d'au plus
/// deux lignes sous le bas de la boîte qui la précède (le menu). Sans trait, ou sans texte au-dessus, tout l'écran compte.
fn body_end(lines: &[&str]) -> usize {
    let has_text = |range: &[&str]| range.iter().any(|line| clean_line(line).is_some());
    let len = lines.len();
    let Some(frame) = lines.iter().rposition(|line| is_frame_line(line)) else {
        return len;
    };
    if is_box_top(lines[frame]) || !has_text(&lines[..frame]) {
        return len;
    }
    let mut end = frame;
    let input_box = lines[..frame]
        .iter()
        .rposition(|line| is_frame_line(line))
        .filter(|&top| is_input_box(&lines[top + 1..frame]) && has_text(&lines[..top]));
    if let Some(top) = input_box {
        end = top;
        if let Some(bottom) = lines[..end].iter().rposition(|line| is_frame_line(line))
            && is_box_bottom(lines[bottom])
            && lines[bottom + 1..end]
                .iter()
                .filter(|line| clean_line(line).is_some())
                .count()
                <= HINT_LINES_MAX
            && has_text(&lines[..bottom])
        {
            end = bottom;
        }
    }
    end
}

/// Marqueurs d'invite de saisie des agents (`❯` Claude Code, `›` Codex, `>` Antigravity).
const PROMPT_MARKERS: [char; 3] = ['❯', '›', '>'];
/// Marqueurs du curseur d'un menu : les marqueurs d'invite, plus les flèches de certains menus.
const CURSOR_MARKERS: [char; 5] = ['❯', '›', '>', '▸', '▶'];
/// Au-delà de ce nombre de blancs de tête, une ligne est alignée à droite (message d'état, pas du texte de l'agent).
const RIGHT_ALIGNED_BLANKS: usize = 40;
/// Une jauge est une suite d'au moins ce nombre de blocs identiques.
const GAUGE_MIN_RUN: usize = 5;

/// La ligne sans ses blancs ni les traits verticaux d'un cadre, aux deux bouts.
fn bare(line: &str) -> &str {
    line.trim_matches(|c: char| c.is_whitespace() || FRAME_VERTICALS.contains(&c))
}

/// Le marqueur de curseur ou d'invite qui ouvre le texte (suivi d'un blanc ou de la fin de ligne), et le reste.
/// `>_ Codex` n'a pas de marqueur : le caractère qui suit n'est pas un blanc.
fn split_marker<'a>(text: &'a str, markers: &[char]) -> Option<&'a str> {
    let rest = text.strip_prefix(markers)?;
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then(|| rest.trim_start())
}

/// Le numéro d'une option de menu (`1. Café`, `❯ 2) Thé`), curseur compris.
fn option_number(line: &str) -> Option<u32> {
    let text = bare(line);
    let text = split_marker(text, &CURSOR_MARKERS).unwrap_or(text);
    let digits = text.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || digits > 3 {
        return None;
    }
    let rest = text[digits..].strip_prefix(['.', ')'])?;
    if !rest.starts_with(char::is_whitespace) || rest.trim().is_empty() {
        return None;
    }
    text[..digits].parse().ok()
}

/// Le rappel de touches d'un menu (`Enter to select · ↑/↓ to navigate · Esc to cancel`, `Press enter to confirm or esc
/// to cancel`). « esc to interrupt » est celui d'un agent au travail, pas d'un menu.
fn is_key_hint(line: &str) -> bool {
    let text = bare(line).to_lowercase();
    !text.contains("interrupt")
        && [
            "enter to ",
            "esc to ",
            "press enter",
            "press esc",
            "tab to ",
            "↑/↓",
            "↑↓",
        ]
        .iter()
        .any(|start| text.starts_with(start))
}

/// Les lignes du menu à choix de l'écran, le plus bas : la question (la ligne utile la plus proche au-dessus de la
/// première option), puis les premières options, `count` lignes au plus. Les lignes de description et le rappel de
/// touches n'y entrent pas. Un menu est de deux sortes : des options numérotées (`1.` ou `1)`, avec un curseur sur l'une
/// d'elles ou un rappel de touches dessous, ce qui le distingue d'une liste numérotée dans un message), ou une ligne à
/// curseur suivie d'options sœurs et d'un rappel de touches.
fn menu_body(lines: &[&str], count: usize) -> Option<Vec<String>> {
    let (first, options) = numbered_menu(lines).or_else(|| cursor_menu(lines))?;
    let question = lines[..first]
        .iter()
        .rev()
        .find_map(|line| clean_line(line));
    let shown: Vec<String> = question
        .into_iter()
        .chain(options.iter().filter_map(|&at| clean_line(lines[at])))
        .take(count)
        .collect();
    Some(shown)
}

/// Les options numérotées consécutives (`N`, `N+1`, …) de la dernière suite d'au moins deux options, si elle a un
/// curseur ou un rappel de touches dessous : l'index de la première option et ceux de toutes.
fn numbered_menu(lines: &[&str]) -> Option<(usize, Vec<usize>)> {
    let mut groups: Vec<Vec<(usize, u32)>> = Vec::new();
    for (at, number) in lines
        .iter()
        .enumerate()
        .filter_map(|(at, line)| option_number(line).map(|number| (at, number)))
    {
        match groups.last_mut() {
            Some(group) if group.last().is_some_and(|&(_, last)| last + 1 == number) => {
                group.push((at, number));
            }
            _ => groups.push(vec![(at, number)]),
        }
    }
    let group = groups.into_iter().rfind(|group| group.len() >= 2)?;
    let last = group.last()?.0;
    let has_cursor = group
        .iter()
        .any(|&(at, _)| split_marker(bare(lines[at]), &CURSOR_MARKERS).is_some());
    let below = &lines[last + 1..];
    let hint_below = below.iter().any(|line| is_key_hint(line));
    // Un curseur seul ne suffit pas : le message passé de Romain (`❯ 1. …`) en porte un lui aussi. Si une invite de
    // saisie ou un message de l'agent suit plus bas, le groupe est de l'historique, pas un menu en attente.
    let superseded = below
        .iter()
        .any(|line| is_plain_prompt(line) || is_agent_message(line));
    (hint_below || (has_cursor && !superseded))
        .then(|| (group[0].0, group.iter().map(|&(at, _)| at).collect()))
}

/// Un menu sans numéros : la dernière ligne à curseur avant le dernier rappel de touches, avec au moins une option sœur
/// entre les deux. Rend l'index de la ligne à curseur et ceux de ses options (elle comprise).
fn cursor_menu(lines: &[&str]) -> Option<(usize, Vec<usize>)> {
    let hint = lines.iter().rposition(|line| is_key_hint(line))?;
    let cursor = lines[..hint].iter().rposition(|line| {
        split_marker(bare(line), &CURSOR_MARKERS)
            .is_some_and(|rest| rest.chars().any(char::is_alphanumeric))
    })?;
    let options: Vec<usize> = (cursor..hint)
        .filter(|&at| clean_line(lines[at]).is_some())
        .collect();
    (options.len() >= 2).then_some((cursor, options))
}

/// Marqueurs d'un message de l'agent (`⏺` Claude Code, `•` Codex).
const MESSAGE_MARKERS: [char; 2] = ['⏺', '•'];

/// Une ligne qui ouvre un message de l'agent.
fn is_agent_message(line: &str) -> bool {
    split_marker(bare(line), &MESSAGE_MARKERS).is_some()
}

/// Une invite non numérotée, vide ou suivie de texte (texte d'exemple compris). Une option à curseur (`❯ 1. Oui`) n'en
/// est pas une : seule la structure de l'écran dit si elle est un menu ou un message passé de Romain.
fn is_plain_prompt(line: &str) -> bool {
    split_marker(bare(line), &PROMPT_MARKERS).is_some_and(|rest| option_number(rest).is_none())
}

/// Une ligne de saisie ou un message passé de Romain : toute ligne dont le texte commence par un marqueur d'invite,
/// numérotée ou non. Ne sert qu'à borner par le haut la région du dernier message, sous une ligne de saisie déjà trouvée
/// (donc sous aucun menu en attente : `menu_body` passe avant).
fn is_prompt_line(line: &str) -> bool {
    split_marker(bare(line), &PROMPT_MARKERS).is_some()
}

/// L'index qui suit une invite et, si elle est numérotée (`❯ 1. …`), les lignes `2.`, `3.`… qui la prolongent.
fn after_prompt(lines: &[&str], at: usize) -> usize {
    let mut end = at + 1;
    let Some(mut number) = option_number(lines[at]) else {
        return end;
    };
    while let Some(next) = lines.get(end).and_then(|line| option_number(line))
        && next == number + 1
    {
        number = next;
        end += 1;
    }
    end
}

/// Une ligne de durée de tour : un mot puis `for <n>s|m|h` (`✻ Cooked for 1s · done 7:33`, `Worked for 1s • 07:28`).
fn is_turn_duration(line: &str) -> bool {
    let text = line.trim_start_matches(|c: char| !c.is_alphanumeric());
    let mut words = text.split_whitespace();
    let (Some(verb), Some("for"), Some(duration)) = (words.next(), words.next(), words.next())
    else {
        return false;
    };
    let digits = duration.trim_end_matches(['s', 'm', 'h']);
    verb.chars().all(char::is_alphabetic)
        && duration.len() > digits.len()
        && !digits.is_empty()
        && digits.chars().all(|c| c.is_ascii_digit())
}

/// Une ligne à jauge : au moins cinq caractères identiques de suite parmi les blocs (U+2580 à U+259F) et les formes
/// géométriques (U+25A0 à U+25FF), `▪▪▪▪▪▪▪▪ 99% hit`.
fn is_gauge(line: &str) -> bool {
    let is_block = |c: char| ('\u{2580}'..='\u{25ff}').contains(&c);
    let mut run = 0;
    let mut previous = None;
    for c in line.chars() {
        run = if previous == Some(c) { run + 1 } else { 1 };
        previous = Some(c);
        if is_block(c) && run >= GAUGE_MIN_RUN {
            return true;
        }
    }
    false
}

/// Un trait qui porte une étiquette (`──── opus-ceres-0piq ─`) : au moins 80 % de caractères de dessin de boîte.
fn is_labelled_frame(line: &str) -> bool {
    let text = line.trim();
    let total = text.chars().count();
    let boxed = text
        .chars()
        .filter(|c| ('\u{2500}'..='\u{257f}').contains(c))
        .count();
    boxed > 0 && boxed * 5 >= total * 4
}

/// Le dernier message de l'agent : les `count` dernières lignes utiles entre l'invite précédente (le dernier message de
/// Romain, avec ses lignes numérotées de suite) et la ligne de saisie, la plus basse de l'écran. Écartées : durées de tour, jauges, lignes alignées à droite et
/// traits étiquetés. Vide sans ligne de saisie, ou quand rien ne reste.
fn last_message(lines: &[&str], count: usize) -> Vec<String> {
    let Some(input) = lines.iter().rposition(|line| is_plain_prompt(line)) else {
        return Vec::new();
    };
    let start = lines[..input]
        .iter()
        .rposition(|line| is_prompt_line(line))
        .map_or(0, |previous| after_prompt(lines, previous));
    let mut found: Vec<String> = lines[start..input]
        .iter()
        .filter(|line| {
            let blanks = line.chars().take_while(|c| c.is_whitespace()).count();
            blanks < RIGHT_ALIGNED_BLANKS
                && !is_turn_duration(line)
                && !is_gauge(line)
                && !is_labelled_frame(line)
        })
        .filter_map(|line| clean_line(line))
        .rev()
        .take(count)
        .collect();
    found.reverse();
    found
}

/// Les `count` lignes utiles de l'écran à montrer dans la notification, dans leur ordre : séquences ANSI retirées, blancs
/// retirés aux deux bouts (et le trait vertical d'un cadre de TUI avec eux), lignes sans lettre ni chiffre écartées,
/// chacune coupée à 110 caractères suivis de « … ». La règle, dans l'ordre :
/// 1. un menu à choix (`menu_body`) : sa question puis ses premières options ;
/// 2. sinon le dernier message de l'agent (`last_message`), au-dessus de la ligne de saisie ;
/// 3. à défaut, les dernières lignes au-dessus du dernier trait horizontal (`body_end`) : le pied de page de l'agent
///    n'est pas lu tant que l'écran a du texte au-dessus ;
/// 4. à défaut, les dernières lignes de l'écran entier.
pub fn useful_lines(screen: &str, count: usize) -> Vec<String> {
    let clean = strip_ansi(screen);
    let lines: Vec<&str> = clean.lines().collect();
    if let Some(menu) = menu_body(&lines, count)
        && !menu.is_empty()
    {
        return menu;
    }
    let message = last_message(&lines, count);
    if !message.is_empty() {
        return message;
    }
    let last = |range: &[&str]| -> Vec<String> {
        let mut found: Vec<String> = range
            .iter()
            .rev()
            .filter_map(|line| clean_line(line))
            .take(count)
            .collect();
        found.reverse();
        found
    };
    let body = last(&lines[..body_end(&lines)]);
    if body.is_empty() { last(&lines) } else { body }
}

/// « {agent} attend · {projet} » ou « {agent} en erreur · {projet} ». L'agent est le premier de la session ; à défaut, le
/// nom affiché de la session. Sans nom de projet, le titre s'arrête au verbe.
pub fn title(
    kind: EpisodeKind,
    agent: Option<&str>,
    display_name: &str,
    project_name: &str,
) -> String {
    let who = agent
        .map(str::trim)
        .filter(|agent| !agent.is_empty())
        .unwrap_or(display_name);
    let verb = match kind {
        EpisodeKind::Waiting => "attend",
        EpisodeKind::Error => "en erreur",
    };
    let project = project_name.trim();
    if project.is_empty() {
        format!("{} {verb}", cut(who, NAME_MAX_CHARS))
    } else {
        format!(
            "{} {verb} · {}",
            cut(who, NAME_MAX_CHARS),
            cut(project, NAME_MAX_CHARS)
        )
    }
}

/// Un morceau d'adresse sûr : seuls les caractères non réservés passent tels quels.
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// La charge utile JSON : titre, corps (les lignes, une par ligne), type, session, pane, badge et adresse à ouvrir au
/// toucher. Si elle dépasse 3 Ko, les lignes sont retirées par la fin, puis le titre raccourci.
pub fn payload(
    kind: EpisodeKind,
    title: &str,
    lines: &[String],
    session: &str,
    pane: u32,
    badge: usize,
) -> String {
    let url = format!("/#/s/{}/{pane}", percent_encode(session));
    let build = |title: &str, lines: &[String]| {
        serde_json::json!({
            "title": title,
            "body": lines.join("\n"),
            "type": kind.as_str(),
            "session": session,
            "pane": pane,
            "badge": badge,
            "url": url,
        })
        .to_string()
    };
    let mut kept = lines.len();
    let mut title = title.to_owned();
    loop {
        let text = build(&title, &lines[..kept]);
        if text.len() <= PAYLOAD_MAX_BYTES || (kept == 0 && title.is_empty()) {
            return text;
        }
        if kept > 0 {
            kept -= 1;
        } else {
            let shorter = title.chars().count() * 3 / 4;
            title = title.chars().take(shorter).collect();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Un menu à la forme de Claude Code, relevée sur de vrais panes (aucun contenu réel) : un cadre `╭─╮│╰─╯` en gris
    /// SGR 38;5;246, un titre en gras, la ligne de la commande, la question, trois choix dont le premier porte `❯`,
    /// puis une ligne d'aide. Les couleurs sont celles de l'agent (indexées 256, gras `1`, remise `0`).
    const CLAUDE_MENU: &str = concat!(
        "\u{1b}[38;5;246m╭──────────────────────────────────────────────────╮\u{1b}[39m\n",
        "\u{1b}[38;5;246m│\u{1b}[39m \u{1b}[1mBash command\u{1b}[0m                                     \u{1b}[38;5;246m│\u{1b}[39m\n",
        "\u{1b}[38;5;246m│\u{1b}[39m   cargo test --workspace                         \u{1b}[38;5;246m│\u{1b}[39m\n",
        "\u{1b}[38;5;246m│\u{1b}[39m   Run the workspace tests                        \u{1b}[38;5;246m│\u{1b}[39m\n",
        "\u{1b}[38;5;246m│\u{1b}[39m                                                  \u{1b}[38;5;246m│\u{1b}[39m\n",
        "\u{1b}[38;5;246m│\u{1b}[39m Do you want to proceed?                          \u{1b}[38;5;246m│\u{1b}[39m\n",
        "\u{1b}[38;5;246m│\u{1b}[39m \u{1b}[38;5;153m❯ 1. Yes\u{1b}[39m                                       \u{1b}[38;5;246m│\u{1b}[39m\n",
        "\u{1b}[38;5;246m│\u{1b}[39m   2. Yes, and don't ask again for cargo test     \u{1b}[38;5;246m│\u{1b}[39m\n",
        "\u{1b}[38;5;246m│\u{1b}[39m   3. No, and tell Claude what to do differently  \u{1b}[38;5;246m│\u{1b}[39m\n",
        "\u{1b}[38;5;246m╰──────────────────────────────────────────────────╯\u{1b}[39m\n",
        "\u{1b}[2m\u{1b}[38;5;246m  Esc to cancel\u{1b}[0m   \n",
        "\n",
        "   \n",
    );

    /// Le pied de page d'un vrai pane de Claude Code, sous le menu : un trait horizontal `─` en gris (SGR 38;5;244), l'invite
    /// `❯` vide, un second trait, une ligne de statut en segments à fond coloré (SGR 48;5;n, caractères de bloc entre les
    /// segments), une ligne de quotas grisée, une ligne de mode en rose. Contenu synthétique, forme relevée.
    const CLAUDE_FOOTER: &str = concat!(
        "\u{1b}[38;5;244m────────────────────────────────────────────────────\u{1b}[39m\n",
        "\u{1b}[39m❯ \n",
        "\u{1b}[38;5;244m────────────────────────────────────────────────────\u{1b}[39m\n",
        "\u{1b}[39m  \u{1b}[38;5;235m\u{1b}[48;5;117m Opus 4 \u{1b}[48;5;215m▏ ctx 12% \u{1b}[48;5;149m▏ main \u{1b}[48;5;59m\u{1b}[38;5;255m 14:02 \u{1b}[39m\u{1b}[49m\n",
        "  \u{1b}[2m\u{1b}[38;5;246mweekly\u{1b}[0m\u{1b}[38;5;246m \u{1b}[38;5;149m41%\u{1b}[38;5;246m - reset 14:00\u{1b}[2m\u{1b}[90m | \u{1b}[38;5;246mdaily\u{1b}[0m\n",
        "  \u{1b}[38;5;211m⏵⏵ accept edits on\u{1b}[38;5;246m (shift+tab to cycle)\u{1b}[39m\n",
    );

    /// Un menu à la forme de Codex, relevée sur un vrai pane de Codex : couleurs 24 bits (`38;2;r;g;b`), `›` devant le
    /// choix courant, lignes d'aide grisées (SGR 2), sans cadre.
    const CODEX_MENU: &str = concat!(
        "\u{1b}[38;2;99;168;248m>_ \u{1b}[1m\u{1b}[39mCodex\u{1b}[0;2m (v9.9.9)\u{1b}[0m\n",
        "\n",
        "  Would you like to run the following command?\n",
        "\n",
        "  \u{1b}[2m$\u{1b}[0m cargo test --workspace\n",
        "\n",
        "\u{1b}[1m› 1. Yes, proceed\u{1b}[0m \u{1b}[2m(y)\u{1b}[0m\n",
        "  2. Yes, and don't ask again for this command \u{1b}[2m(p)\u{1b}[0m\n",
        "  3. No, and tell Codex what to do differently \u{1b}[2m(esc)\u{1b}[0m\n",
        "\n",
        "\u{1b}[2m  Press enter to confirm or esc to cancel\u{1b}[0m\n",
        "\n",
    );

    #[test]
    fn titles() {
        assert_eq!(
            title(EpisodeKind::Waiting, Some("Codex"), "Refonte", "erom-seo"),
            "Codex attend · erom-seo"
        );
        assert_eq!(
            title(EpisodeKind::Error, Some("Claude"), "Refonte", "erom-seo"),
            "Claude en erreur · erom-seo"
        );
        // Pas d'agent connu : le nom affiché de la session fait l'affaire.
        assert_eq!(
            title(EpisodeKind::Waiting, None, "Refonte du site", "erom-seo"),
            "Refonte du site attend · erom-seo"
        );
        assert_eq!(
            title(EpisodeKind::Error, Some(""), "Refonte", "erom-seo"),
            "Refonte en erreur · erom-seo"
        );
        // Pas de projet (un dossier sans nom) : le titre ne traîne pas de séparateur.
        assert_eq!(
            title(EpisodeKind::Waiting, Some("Codex"), "x", ""),
            "Codex attend"
        );
        // Un nom démesuré ne gonfle pas la charge utile.
        let long = "é".repeat(500);
        let cut = title(EpisodeKind::Waiting, Some(&long), "x", &long);
        assert!(cut.chars().count() < 200, "{}", cut.chars().count());
        assert!(cut.contains(" attend · "));
    }

    #[test]
    fn strip_ansi_removes_sequences_and_nothing_else() {
        assert_eq!(
            strip_ansi("\u{1b}[1mgras\u{1b}[0m et \u{1b}[38;5;153mbleu\u{1b}[39m"),
            "gras et bleu"
        );
        assert_eq!(strip_ansi("\u{1b}[38;2;99;168;248m>_\u{1b}[0m"), ">_");
        assert_eq!(strip_ansi("a\u{1b}[2K\u{1b}[1;1Hb\u{1b}[?25lc"), "abc");
        // Paramètres à la manière de xterm (deux-points), intermédiaires, caractère final quelconque.
        assert_eq!(strip_ansi("\u{1b}[4:3mx\u{1b}[0 qy"), "xy");
        // Titre de fenêtre et lien hypertexte : terminés par BEL, ou par ST (ESC puis antislash).
        assert_eq!(strip_ansi("\u{1b}]0;un titre\u{7}texte"), "texte");
        assert_eq!(
            strip_ansi("\u{1b}]8;;https://example.org\u{1b}\\lien\u{1b}]8;;\u{1b}\\ fin"),
            "lien fin"
        );
        // Jeux de caractères et autres échappements d'un octet.
        assert_eq!(strip_ansi("\u{1b}(Bx\u{1b}=y\u{1b}7z"), "xyz");
        // Séquences tronquées en fin de texte : elles disparaissent, sans rien emporter d'avant.
        assert_eq!(strip_ansi("abc\u{1b}[38;5;"), "abc");
        assert_eq!(strip_ansi("abc\u{1b}"), "abc");
        assert_eq!(strip_ansi("abc\u{1b}]0;titre sans fin"), "abc");
        // Une séquence cassée par une fin de ligne n'emporte pas la ligne suivante.
        assert_eq!(strip_ansi("a\u{1b}[31\nb"), "a\nb");
        assert_eq!(strip_ansi("a\u{1b}]0;titre\nb"), "a\nb");
        // Les contrôles disparaissent, la fin de ligne et la tabulation restent ; les C1 aussi disparaissent.
        assert_eq!(
            strip_ansi("a\u{7}b\u{8}c\rd\u{7f}e\u{85}f\n\tg"),
            "abcdef\n\tg"
        );
        // Le texte, accents, cadres et symboles compris, passe tel quel ; un marqueur HTML reste du texte.
        assert_eq!(strip_ansi("│ ❯ 1. Oui, é à ç ✻ │"), "│ ❯ 1. Oui, é à ç ✻ │");
        assert_eq!(
            strip_ansi("<img src=x onerror=alert(1)>"),
            "<img src=x onerror=alert(1)>"
        );
    }

    #[test]
    fn useful_lines_skip_frames_and_colors() {
        // Menu à la forme de Claude Code, sans pied de page : le trait du bas du cadre ferme le corps, le rappel de touches
        // dessous n'est pas lu, le trait vertical du cadre ne reste pas collé aux lignes.
        assert_eq!(
            useful_lines(CLAUDE_MENU, 4),
            [
                "Do you want to proceed?",
                "❯ 1. Yes",
                "2. Yes, and don't ask again for cargo test",
                "3. No, and tell Claude what to do differently",
            ]
        );
        assert_eq!(
            useful_lines(CLAUDE_MENU, 3),
            [
                "Do you want to proceed?",
                "❯ 1. Yes",
                "2. Yes, and don't ask again for cargo test",
            ]
        );
        // Menu à la forme de Codex (couleurs 24 bits, aide grisée, `›` du choix courant, aucun cadre) : sa question et
        // ses premiers choix, jamais la ligne d'aide.
        assert_eq!(
            useful_lines(CODEX_MENU, 3),
            [
                "$ cargo test --workspace",
                "› 1. Yes, proceed (y)",
                "2. Yes, and don't ask again for this command (p)",
            ]
        );
        assert_eq!(
            useful_lines(CODEX_MENU, 7),
            [
                "$ cargo test --workspace",
                "› 1. Yes, proceed (y)",
                "2. Yes, and don't ask again for this command (p)",
                "3. No, and tell Codex what to do differently (esc)",
            ]
        );
        // Aucune séquence d'échappement ni caractère de contrôle ne survit dans ce qui est rendu.
        for line in useful_lines(CLAUDE_MENU, 20)
            .into_iter()
            .chain(useful_lines(CODEX_MENU, 20))
        {
            assert!(!line.chars().any(char::is_control), "{line:?}");
        }
    }

    /// Un écran entier de Claude Code : le menu, son rappel de touches, la boîte de saisie vide, puis le pied de page
    /// (statut, quotas, mode). On lit la question et les choix, pas le pied de page.
    #[test]
    fn the_agent_footer_is_not_read_when_there_is_a_question_above() {
        let screen = format!("{CLAUDE_MENU}{CLAUDE_FOOTER}");
        assert_eq!(
            useful_lines(&screen, 4),
            [
                "Do you want to proceed?",
                "❯ 1. Yes",
                "2. Yes, and don't ask again for cargo test",
                "3. No, and tell Claude what to do differently",
            ]
        );
        assert_eq!(
            useful_lines(&screen, 3),
            [
                "Do you want to proceed?",
                "❯ 1. Yes",
                "2. Yes, and don't ask again for cargo test",
            ]
        );
        assert_eq!(
            useful_lines(&screen, 20),
            [
                "Do you want to proceed?",
                "❯ 1. Yes",
                "2. Yes, and don't ask again for cargo test",
                "3. No, and tell Claude what to do differently",
            ]
        );
    }

    /// Un pane de Claude Code au repos, sans menu : la fin du message de l'agent, au-dessus de la boîte de saisie vide.
    #[test]
    fn an_idle_claude_pane_shows_the_end_of_the_message() {
        let screen = format!(
            "\u{1b}[1mLes tests passent.\u{1b}[0m\n\nVeux-tu que je committe ?\n\n{CLAUDE_FOOTER}"
        );
        assert_eq!(
            useful_lines(&screen, 3),
            ["Les tests passent.", "Veux-tu que je committe ?"]
        );
    }

    // Vrais écrans capturés par `tmux capture-pane -e -p` (nettoyés de toute donnée personnelle).
    const CLAUDE_IDLE_ANSWER: &str = include_str!("testdata/screens/claude-idle-answer.ansi");
    const CLAUDE_IDLE_UPDATE: &str = include_str!("testdata/screens/claude-idle-update.ansi");
    const CLAUDE_ASKUSER: &str = include_str!("testdata/screens/claude-askuser.ansi");
    const CLAUDE_PERMISSION: &str = include_str!("testdata/screens/claude-permission.ansi");
    const CLAUDE_TRUST_MENU: &str = include_str!("testdata/screens/claude-trust-menu.ansi");
    const CODEX_QUESTION: &str = include_str!("testdata/screens/codex-question.ansi");
    const AGY_IDLE: &str = include_str!("testdata/screens/agy-idle.ansi");

    #[test]
    fn a_real_idle_claude_pane_shows_its_last_message_not_its_status_widgets() {
        // Durée du tour, widget à jauge, ligne de mise à jour alignée à droite, trait étiqueté : rien n'entre.
        assert_eq!(useful_lines(CLAUDE_IDLE_ANSWER, 3), ["⏺ ok"]);
        assert_eq!(useful_lines(CLAUDE_IDLE_UPDATE, 3), ["⏺ ok"]);
    }

    #[test]
    fn a_real_claude_menu_shows_its_question_then_its_first_options() {
        // AskUserQuestion : un trait simple ouvre le menu, des lignes de description s'intercalent, un trait coupe les
        // options, le rappel de touches est dessous.
        assert_eq!(
            useful_lines(CLAUDE_ASKUSER, 3),
            ["Café ou thé ?", "❯ 1. Café", "2. Thé"]
        );
        // Permission : la suite de l'option 2 est une ligne de description.
        assert_eq!(
            useful_lines(CLAUDE_PERMISSION, 3),
            [
                "Do you want to create hello.txt?",
                "❯ 1. Yes",
                "2. Yes, and switch to accept edits (auto-approve file edits and common file commands) for this",
            ]
        );
        // Plus de place : toutes les options, jamais les descriptions ni le rappel de touches.
        assert_eq!(
            useful_lines(CLAUDE_ASKUSER, 10),
            [
                "Café ou thé ?",
                "❯ 1. Café",
                "2. Thé",
                "3. Type something.",
                "4. Chat about this"
            ]
        );
    }

    #[test]
    fn a_menu_without_numbers_is_read_by_its_cursor_and_its_key_reminder() {
        let lines = useful_lines(CLAUDE_TRUST_MENU, 3);
        assert_eq!(
            lines,
            ["Security guide", "❯ No, exit", "Yes, I trust this folder"]
        );
        assert!(
            useful_lines(CLAUDE_TRUST_MENU, 20)
                .iter()
                .all(|line| !line.contains("Enter to confirm"))
        );
    }

    #[test]
    fn a_real_codex_pane_shows_its_last_message_not_its_input_line_and_status() {
        assert_eq!(
            useful_lines(CODEX_QUESTION, 3),
            ["• Tu préfères quelle couleur ?", "• 🔴 Rouge", "• 🔵 Bleu"]
        );
    }

    #[test]
    fn a_real_agy_pane_shows_its_answer_without_the_previous_prompt() {
        assert_eq!(
            useful_lines(AGY_IDLE, 3),
            [
                "Salut Alex ! Prêt à en découdre. Sur quoi",
                "est-ce qu'on se lance aujourd'hui ?"
            ]
        );
    }

    #[test]
    fn a_numbered_list_in_an_answer_is_not_a_menu() {
        // Ni curseur ni rappel de touches : la liste fait partie du message, la boîte de saisie vide est dessous.
        let screen = "❯ liste\n\nVoici :\n1. un\n2. deux\n\n────────\n❯ \n────────\n  statut\n";
        assert_eq!(useful_lines(screen, 3), ["Voici :", "1. un", "2. deux"]);
    }

    #[test]
    fn a_past_numbered_claude_prompt_is_not_a_menu() {
        // Le message passé de Romain porte un `❯` et deux lignes numérotées, puis la réponse, puis la boîte de saisie vide.
        let screen = "❯ 1. corrige le parseur\n  2. corrige le test\n\n⏺ Fait.\n\n────────\n❯ \n────────\n  statut\n";
        assert_eq!(useful_lines(screen, 3), ["⏺ Fait."]);
        // Même chose quand la boîte de saisie porte déjà du texte.
        let typed = "❯ 1. corrige le parseur\n  2. corrige le test\n\n⏺ Fait.\n\n────────\n❯ et le README\n────────\n  statut\n";
        assert_eq!(useful_lines(typed, 3), ["⏺ Fait."]);
    }

    #[test]
    fn a_past_numbered_codex_prompt_is_not_a_menu() {
        let screen = "› 1. corrige le parseur\n  2. corrige le test\n\n• Fait.\n\n› Ask Codex to do anything\n\n  GPT-6-Luna · project\n";
        assert_eq!(useful_lines(screen, 3), ["• Fait."]);
    }

    #[test]
    fn a_past_numbered_agy_prompt_is_not_a_menu() {
        // Antigravity : pas de marqueur de message, seule l'invite vide plus bas dit que le groupe est passé.
        let screen = "> 1. corrige le parseur\n  2. corrige le test\n\n  Fait.\n\n────────\n>\n────────\n? for shortcuts\n";
        assert_eq!(useful_lines(screen, 3), ["Fait."]);
    }

    #[test]
    fn a_numbered_menu_with_a_cursor_stays_a_menu_below_an_old_prompt() {
        // Le vrai menu est le plus bas : l'invite passée au-dessus ne le disqualifie pas, le rappel de touches le confirme.
        let screen =
            "❯ corrige\n\n⏺ Fait.\n\nQuelle suite ?\n❯ 1. Oui\n  2. Non\n\nEsc to cancel\n";
        assert_eq!(
            useful_lines(screen, 3),
            ["Quelle suite ?", "❯ 1. Oui", "2. Non"]
        );
    }

    #[test]
    fn turn_durations_gauges_right_aligned_lines_and_labelled_frames_are_not_message_lines() {
        let aligned = format!("{}✔ Update installed", " ".repeat(60));
        let labelled = format!("{} opus-ceres-0piq ─", "─".repeat(80));
        let screen = format!(
            "❯ question\n\nréponse\nCooked for 1m · done\n  Worked for 12s • 07:28\n▪▪▪▪▪ 99%\n{aligned}\n{labelled}\n❯ \n"
        );
        assert_eq!(useful_lines(&screen, 5), ["réponse"]);
        // Quatre blocs identiques ne font pas une jauge ; « for » sans durée ne fait pas une durée de tour.
        let kept = "❯ q\n▪▪▪▪ 4 blocs\nWaited for you\n❯ \n";
        assert_eq!(useful_lines(kept, 5), ["▪▪▪▪ 4 blocs", "Waited for you"]);
    }

    #[test]
    fn a_screen_with_nothing_readable_above_the_input_line_falls_back_to_the_old_rule() {
        // Région vide (seules des lignes de statut) : repli sur la règle du dernier trait, puis sur l'écran entier.
        let screen = "Cooked for 1s\n────────\n❯ \n────────\nstatut\n";
        assert_eq!(useful_lines(screen, 3), ["Cooked for 1s"]);
        assert_eq!(useful_lines("Worked for 1s\n❯ \n", 3), ["Worked for 1s"]);
    }

    #[test]
    fn a_screen_without_frames_keeps_its_last_three_lines() {
        let shell = "$ ls\nfile_a\nfile_b\nfile_c\n$ \n";
        assert_eq!(useful_lines(shell, 3), ["file_a", "file_b", "file_c"]);
        // Un trait `-----` en ASCII n'est pas un cadre : il n'a ni lettre ni chiffre, il est écarté comme le reste.
        assert_eq!(useful_lines("avant\n-----\napres\n", 3), ["avant", "apres"]);
    }

    #[test]
    fn a_screen_whose_only_text_is_below_the_frame_falls_back_to_the_current_rule() {
        // Rien au-dessus du trait : la règle s'applique à l'écran entier.
        assert_eq!(
            useful_lines("────────\nstatut un\nstatut deux\n", 3),
            ["statut un", "statut deux"]
        );
        // Une boîte de saisie vide sans rien de lisible au-dessus : repli sur l'écran entier.
        assert_eq!(
            useful_lines("────────\n❯\n────────\nstatut un\nstatut deux\n", 3),
            ["statut un", "statut deux"]
        );
        // Le haut d'une boîte n'est pas un pied de page : ce qui est dessous en fait partie.
        assert_eq!(
            useful_lines("avant\n╭──────╮\n│ Question ? │\n│ ❯ 1. Oui │\n", 3),
            ["avant", "Question ?", "❯ 1. Oui"]
        );
    }

    #[test]
    fn useful_lines_drop_everything_without_a_letter_or_a_digit() {
        let screen = "╭────╮\n│    │\n╰────╯\n  ❯  \n────────\n   \n\u{1b}[2m...\u{1b}[0m\n";
        assert!(useful_lines(screen, 3).is_empty());
        assert!(useful_lines("", 3).is_empty());
        assert!(useful_lines("seule\n", 0).is_empty());
        // Moins de lignes utiles que demandé : celles qu'il y a, dans l'ordre.
        assert_eq!(useful_lines("un\n\ndeux\n", 3), ["un", "deux"]);
        // Une lettre accentuée ou un chiffre suffisent ; une fin de ligne Windows ou des tabulations ne gênent pas.
        assert_eq!(useful_lines("é\r\n\t7\t\n", 3), ["é", "7"]);
        // Un marqueur HTML reste du texte, rendu tel quel.
        assert_eq!(
            useful_lines("<img src=x onerror=alert(1)>\n", 1),
            ["<img src=x onerror=alert(1)>"]
        );
    }

    #[test]
    fn lines_are_cut_at_110() {
        let line = "a".repeat(200);
        let lines = useful_lines(&line, 1);
        assert_eq!(lines[0].chars().count(), 111);
        assert_eq!(lines[0], format!("{}…", "a".repeat(110)));
        // Pile 110 caractères : rien n'est coupé ; 111 : coupé. Les caractères comptent, pas les octets.
        let exact = "é".repeat(110);
        assert_eq!(useful_lines(&exact, 1)[0], exact);
        let over = "é".repeat(111);
        assert_eq!(useful_lines(&over, 1)[0], format!("{}…", "é".repeat(110)));
        // Une ligne de 300 colonnes avec une couleur au milieu : la coupe se fait sur le texte, pas sur le balisage.
        let colored = format!("{}\u{1b}[31m{}\u{1b}[0m", "b".repeat(100), "c".repeat(200));
        assert_eq!(
            useful_lines(&colored, 1)[0],
            format!("{}{}…", "b".repeat(100), "c".repeat(10))
        );
    }

    #[test]
    fn payload_carries_what_the_service_worker_needs() {
        let lines = vec!["Do you want to proceed?".to_owned(), "1. Yes".to_owned()];
        let text = payload(
            EpisodeKind::Waiting,
            "Claude attend · erom-seo",
            &lines,
            "idema-ab12cd34",
            7,
            3,
        );
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["title"], "Claude attend · erom-seo");
        assert_eq!(json["body"], "Do you want to proceed?\n1. Yes");
        assert_eq!(json["type"], "waiting");
        assert_eq!(json["session"], "idema-ab12cd34");
        assert_eq!(json["pane"], 7);
        assert_eq!(json["badge"], 3);
        assert_eq!(json["url"], "/#/s/idema-ab12cd34/7");
        let error = payload(EpisodeKind::Error, "t", &[], "idema-ab12cd34", 1, 0);
        let json: serde_json::Value = serde_json::from_str(&error).unwrap();
        assert_eq!(json["type"], "error");
        assert_eq!(json["body"], "");
        // Un nom de session hors de l'ordinaire reste un chemin sûr dans l'adresse.
        let odd = payload(EpisodeKind::Waiting, "t", &[], "a b/c?d#e", 1, 0);
        let json: serde_json::Value = serde_json::from_str(&odd).unwrap();
        assert_eq!(json["url"], "/#/s/a%20b%2Fc%3Fd%23e/1");
        assert_eq!(json["session"], "a b/c?d#e");
    }

    #[test]
    fn payload_stays_under_3kb() {
        // Trois lignes ordinaires, même toutes en lettres de quatre octets, tiennent sans rien retirer.
        let wide = "𝔸".repeat(110);
        let lines: Vec<String> = useful_lines(&format!("{wide}\n{wide}\n{wide}"), 3);
        let text = payload(EpisodeKind::Waiting, "t", &lines, "idema-ab12cd34", 1, 1);
        assert!(text.len() <= PAYLOAD_MAX_BYTES, "{}", text.len());
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["body"].as_str().unwrap().lines().count(), 3);

        // Des lignes qui ne tiennent pas : retirées par la fin jusqu'à ce que la charge entre, les premières restent.
        let heavy: Vec<String> = (0..6).map(|n| format!("{n}{}", "x".repeat(799))).collect();
        let text = payload(EpisodeKind::Error, "t", &heavy, "idema-ab12cd34", 1, 1);
        assert!(text.len() <= PAYLOAD_MAX_BYTES, "{}", text.len());
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        let kept: Vec<&str> = json["body"].as_str().unwrap().lines().collect();
        assert!(
            !kept.is_empty() && kept.len() < heavy.len(),
            "{}",
            kept.len()
        );
        assert!(kept.iter().zip(&heavy).all(|(kept, line)| kept == line));

        // Même un titre démesuré ne fait pas dépasser.
        let huge = "é".repeat(5000);
        let text = payload(EpisodeKind::Waiting, &huge, &heavy, "idema-ab12cd34", 1, 1);
        assert!(text.len() <= PAYLOAD_MAX_BYTES, "{}", text.len());
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(json["title"].as_str().unwrap().starts_with('é'));
    }
}
