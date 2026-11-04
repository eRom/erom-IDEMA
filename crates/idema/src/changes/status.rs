//! Lecture de `git status --porcelain=v2 -z --untracked-files=all --branch`.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use super::{BranchInfo, Change, Check, Group, Letter};

/// Lit la sortie de `status --porcelain=v2 -z --untracked-files=all --branch` : la branche, puis les changements dans
/// l'ordre de Git. Avec `-z`, les chemins ne sont pas entre guillemets et un renommage porte son ancien chemin dans
/// l'enregistrement suivant.
pub fn parse(bytes: &[u8]) -> Result<(BranchInfo, Vec<Change>), String> {
    let mut branch = BranchInfo {
        head: None,
        has_commits: true,
        upstream: None,
        upstream_gone: false,
        ahead: 0,
    };
    let mut has_ahead_behind = false;
    let mut changes = Vec::new();
    let mut records = bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty());
    while let Some(record) = records.next() {
        match record[0] {
            b'#' => parse_header(record, &mut branch, &mut has_ahead_behind)?,
            b'1' => {
                let fields = split_fields(record, 9)?;
                changes.push(tracked_change(fields[1], path_of(fields[8]), None, false)?);
            }
            b'2' => {
                let fields = split_fields(record, 10)?;
                let orig_path = records
                    .next()
                    .ok_or_else(|| "rename record without its original path".to_string())?;
                changes.push(tracked_change(
                    fields[1],
                    path_of(fields[9]),
                    Some(path_of(orig_path)),
                    true,
                )?);
            }
            b'u' => {
                let fields = split_fields(record, 11)?;
                changes.push(Change {
                    path: path_of(fields[10]),
                    orig_path: None,
                    group: Group::Conflicts,
                    letter: Letter::Conflict,
                    check: Check::NoBox,
                });
            }
            b'?' => {
                let fields = split_fields(record, 2)?;
                changes.push(Change {
                    path: path_of(fields[1]),
                    orig_path: None,
                    group: Group::Untracked,
                    letter: Letter::Untracked,
                    check: Check::Unchecked,
                });
            }
            // Un fichier ignoré : demandé jamais, ignoré s'il vient quand même.
            b'!' => {}
            _ => {
                return Err(format!(
                    "unknown status record: {}",
                    String::from_utf8_lossy(record)
                ));
            }
        }
    }
    // Sans `branch.ab`, Git ne sait pas comparer à l'amont : la branche distante suivie n'existe plus.
    branch.upstream_gone = branch.upstream.is_some() && !has_ahead_behind;
    Ok((branch, changes))
}

fn parse_header(
    record: &[u8],
    branch: &mut BranchInfo,
    has_ahead_behind: &mut bool,
) -> Result<(), String> {
    let line = String::from_utf8_lossy(record);
    let Some((key, value)) = line
        .strip_prefix("# ")
        .and_then(|rest| rest.split_once(' '))
    else {
        return Ok(());
    };
    match key {
        "branch.oid" => branch.has_commits = value != "(initial)",
        "branch.head" => branch.head = (value != "(detached)").then(|| value.to_string()),
        "branch.upstream" => branch.upstream = Some(value.to_string()),
        "branch.ab" => {
            *has_ahead_behind = true;
            branch.ahead = value
                .split(' ')
                .next()
                .and_then(|ahead| ahead.strip_prefix('+'))
                .and_then(|ahead| ahead.parse().ok())
                .ok_or_else(|| format!("malformed branch.ab: {value}"))?;
        }
        _ => {}
    }
    Ok(())
}

/// Les `count` premiers champs séparés par une espace ; le dernier garde ses espaces (c'est le chemin).
fn split_fields(record: &[u8], count: usize) -> Result<Vec<&[u8]>, String> {
    let fields: Vec<&[u8]> = record.splitn(count, |byte| *byte == b' ').collect();
    if fields.len() == count {
        Ok(fields)
    } else {
        Err(format!(
            "status record with too few fields: {}",
            String::from_utf8_lossy(record)
        ))
    }
}

fn path_of(bytes: &[u8]) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(bytes))
}

/// Un changement des lignes `1` et `2`, à partir de leur `XY`.
fn tracked_change(
    xy: &[u8],
    path: PathBuf,
    orig_path: Option<PathBuf>,
    renamed: bool,
) -> Result<Change, String> {
    let [x, y] = xy else {
        return Err(format!("malformed XY: {}", String::from_utf8_lossy(xy)));
    };
    let check = match (*x != b'.', *y != b'.') {
        (true, false) => Check::Checked,
        (true, true) => Check::Mixed,
        _ => Check::Unchecked,
    };
    let letter = if [*x, *y].contains(&b'A') {
        Letter::Added
    } else if [*x, *y].contains(&b'D') {
        Letter::Deleted
    } else if renamed {
        Letter::Renamed
    } else {
        Letter::Modified
    };
    Ok(Change {
        path,
        orig_path,
        group: Group::Modified,
        letter,
        check,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::{Check, Group, Letter};
    use std::path::PathBuf;

    /// Les enregistrements d'une sortie `-z` : chacun finit par un octet nul.
    fn nul_joined(records: &[&str]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for record in records {
            bytes.extend_from_slice(record.as_bytes());
            bytes.push(0);
        }
        bytes
    }

    const HASHES: &str = "100644 100644 100644 aaaa bbbb";

    fn tracked(xy: &str, path: &str) -> String {
        format!("1 {xy} N... {HASHES} {path}")
    }

    #[test]
    fn parses_the_branch_header() {
        let bytes = nul_joined(&[
            "# branch.oid 0123456789abcdef",
            "# branch.head main",
            "# branch.upstream origin/main",
            "# branch.ab +2 -0",
        ]);
        let (branch, changes) = parse(&bytes).unwrap();
        assert_eq!(branch.head.as_deref(), Some("main"));
        assert_eq!(branch.upstream.as_deref(), Some("origin/main"));
        assert_eq!(branch.ahead, 2);
        assert!(!branch.upstream_gone);
        assert!(branch.has_commits);
        assert!(changes.is_empty());

        let detached = nul_joined(&["# branch.oid 0123456789abcdef", "# branch.head (detached)"]);
        let (branch, _) = parse(&detached).unwrap();
        assert_eq!(branch.head, None);
        assert_eq!(branch.upstream, None);
        assert!(!branch.upstream_gone);
        assert_eq!(branch.ahead, 0);

        // L'amont est noté, mais Git ne donne pas d'avance : la branche distante a été supprimée.
        let gone = nul_joined(&[
            "# branch.oid 0123456789abcdef",
            "# branch.head feat",
            "# branch.upstream origin/feat",
        ]);
        let (branch, _) = parse(&gone).unwrap();
        assert_eq!(branch.upstream.as_deref(), Some("origin/feat"));
        assert!(branch.upstream_gone);

        let unborn = nul_joined(&["# branch.oid (initial)", "# branch.head main"]);
        let (branch, _) = parse(&unborn).unwrap();
        assert!(!branch.has_commits);
        assert_eq!(branch.head.as_deref(), Some("main"));
    }

    #[test]
    fn parses_renames_spaces_and_accents() {
        let rename = format!("2 R. N... {HASHES} R100 notes/été 2.md");
        let bytes = nul_joined(&[
            "# branch.head main",
            &rename,
            "notes/old name.md",
            "? captures/a b.png",
        ]);
        let (_, changes) = parse(&bytes).unwrap();
        assert_eq!(changes.len(), 2);

        let renamed = &changes[0];
        assert_eq!(renamed.path, PathBuf::from("notes/été 2.md"));
        assert_eq!(renamed.orig_path, Some(PathBuf::from("notes/old name.md")));
        assert_eq!(renamed.letter, Letter::Renamed);
        assert_eq!(renamed.check, Check::Checked);
        assert_eq!(renamed.group, Group::Modified);

        let untracked = &changes[1];
        assert_eq!(untracked.path, PathBuf::from("captures/a b.png"));
        assert_eq!(untracked.orig_path, None);
        assert_eq!(untracked.group, Group::Untracked);
        assert_eq!(untracked.letter, Letter::Untracked);
        assert_eq!(untracked.check, Check::Unchecked);
    }

    #[test]
    fn parses_check_states() {
        let unmerged = format!(
            "u UU N... 100644 100644 100644 100644 aaaa bbbb cccc {}",
            "conflit.md"
        );
        let records = [
            tracked("M.", "a"),
            tracked(".M", "b"),
            tracked("MM", "c"),
            tracked("D.", "d"),
            tracked(".D", "e"),
            tracked("AM", "f"),
            unmerged,
        ];
        let refs: Vec<&str> = records.iter().map(String::as_str).collect();
        let (_, changes) = parse(&nul_joined(&refs)).unwrap();

        let checks: Vec<Check> = changes.iter().map(|change| change.check).collect();
        assert_eq!(
            checks,
            [
                Check::Checked,
                Check::Unchecked,
                Check::Mixed,
                Check::Checked,
                Check::Unchecked,
                Check::Mixed,
                Check::NoBox,
            ]
        );
        let letters: Vec<&str> = changes.iter().map(|change| change.letter.glyph()).collect();
        assert_eq!(letters, ["M", "M", "M", "D", "D", "A", "!"]);
        assert_eq!(changes[6].group, Group::Conflicts);
        assert_eq!(changes[0].group, Group::Modified);
    }

    #[test]
    fn a_path_that_is_not_utf8_survives() {
        use std::os::unix::ffi::OsStrExt;
        let mut record = b"? caf".to_vec();
        record.push(0xE9);
        record.extend_from_slice(b".txt");
        record.push(0);
        let (_, changes) = parse(&record).unwrap();
        assert_eq!(
            changes[0].path.as_os_str().as_bytes(),
            &record[2..record.len() - 1]
        );
    }

    #[test]
    fn a_malformed_record_is_an_error_not_a_panic() {
        assert!(parse(&nul_joined(&["1 M."])).is_err());
        assert!(parse(&nul_joined(&["# branch.ab nonsense"])).is_err());
        assert!(parse(&nul_joined(&["Z what is this"])).is_err());
        let rename_without_origin = format!("2 R. N... {HASHES} R100 new.md");
        assert!(parse(&nul_joined(&[&rename_without_origin])).is_err());
    }
}
