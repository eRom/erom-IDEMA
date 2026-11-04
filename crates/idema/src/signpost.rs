//! Repères `os_signpost` (catégorie Points of Interest) pour mesurer la latence d'écho dans Instruments.
//!
//! Idema peint à la demande : une frappe isolée ne produit qu'une image présentée, quelle que soit la latence.
//! La piste de présentation de Metal System Trace ne peut donc pas, seule, révéler le déclencheur du §12.
//! L'intervalle `echo` va de l'arrivée de `%output` pour le pane à la fin de la peinture qui le montre :
//! on le lit à côté de la piste d'images présentées pour compter les images entre l'arrivée et l'affichage.
//!
//! Pas de crate : `signpost-rs` n'a pas bougé depuis 2022 et n'est pas publié, `apple-log` exige un pont Swift.
//! Les macros `os_signpost_*` du SDK se réduisent à `_os_signpost_emit_with_name_impl`, symbole exporté et déclaré
//! dans `<os/signpost.h>` : on l'appelle directement. Le coût hors enregistrement est un test `os_signpost_enabled`.

use std::ffi::{CStr, c_char, c_void};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

const SUBSYSTEM: &CStr = c"com.erom.idema";
/// `OS_LOG_CATEGORY_POINTS_OF_INTEREST` : la catégorie que Instruments affiche sans réglage.
const CATEGORY: &CStr = c"PointsOfInterest";
const ECHO_NAME: &CStr = c"echo";
/// Un seul intervalle `echo` est ouvert à la fois : l'identifiant doit seulement n'être ni nul ni invalide.
const ECHO_ID: u64 = 1;

const OS_SIGNPOST_INTERVAL_BEGIN: u8 = 1;
const OS_SIGNPOST_INTERVAL_END: u8 = 2;

type OsLog = *mut c_void;

unsafe extern "C" {
    /// Fourni par l'éditeur de liens : identifie l'image Mach-O de l'exécutable.
    static __dso_handle: u8;
    fn os_log_create(subsystem: *const c_char, category: *const c_char) -> OsLog;
    fn os_signpost_enabled(log: OsLog) -> bool;
    fn _os_signpost_emit_with_name_impl(
        dso: *const c_void,
        log: OsLog,
        kind: u8,
        id: u64,
        name: *const c_char,
        format: *const c_char,
        buffer: *mut u8,
        size: u32,
    );
}

/// Le journal `os_log_t` est sûr entre fils : Apple le documente comme utilisable de n'importe quel fil.
struct Log(OsLog);

unsafe impl Send for Log {}
unsafe impl Sync for Log {}

static LOG: OnceLock<Log> = OnceLock::new();
/// Vrai tant qu'un intervalle `echo` est ouvert : l'arrivée l'ouvre, la peinture le ferme.
static ECHO_OPEN: AtomicBool = AtomicBool::new(false);

fn emit(kind: u8) {
    let log = LOG
        .get_or_init(|| Log(unsafe { os_log_create(SUBSYSTEM.as_ptr(), CATEGORY.as_ptr()) }))
        .0;
    if log.is_null() || !unsafe { os_signpost_enabled(log) } {
        return;
    }
    // Format vide : le tampon d'arguments d'`os_log` se réduit à l'octet de résumé et au nombre d'arguments.
    let mut buffer = [0u8; 2];
    unsafe {
        _os_signpost_emit_with_name_impl(
            (&raw const __dso_handle).cast(),
            log,
            kind,
            ECHO_ID,
            ECHO_NAME.as_ptr(),
            c"".as_ptr(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
        );
    }
}

/// Arrivée de `%output` pour le pane. Ouvre l'intervalle s'il ne l'est pas déjà (une rafale ne le rouvre pas,
/// comme la mesure du HUD). Appelable depuis le fil de lecture. Vrai si l'intervalle vient de s'ouvrir.
pub fn echo_arrived() -> bool {
    let opened = !ECHO_OPEN.swap(true, Ordering::AcqRel);
    if opened {
        emit(OS_SIGNPOST_INTERVAL_BEGIN);
    }
    opened
}

/// Fin de la peinture qui montre la sortie. Ferme l'intervalle s'il est ouvert. Vrai si l'intervalle vient de se fermer.
pub fn echo_painted() -> bool {
    let closed = ECHO_OPEN.swap(false, Ordering::AcqRel);
    if closed {
        emit(OS_SIGNPOST_INTERVAL_END);
    }
    closed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echo_interval_opens_once_per_burst_and_closes_on_paint() {
        assert!(echo_arrived());
        assert!(!echo_arrived(), "a burst must not reopen the interval");
        assert!(echo_painted());
        assert!(
            !echo_painted(),
            "a paint without pending output closes nothing"
        );
        assert!(echo_arrived(), "the next output opens a new interval");
        assert!(echo_painted());
    }
}
