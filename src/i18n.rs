//! Interface language: English by default, Brazilian Portuguese when the system (or the
//! user, in Settings) asks for it. The .slint texts use Slint's bundled translations
//! (lang/pt_BR/LC_MESSAGES/linex-mods.po); messages built in Rust use `tr!`.

use std::sync::atomic::{AtomicBool, Ordering};

static PT: AtomicBool = AtomicBool::new(false);

pub fn is_pt() -> bool {
    PT.load(Ordering::Relaxed)
}

pub fn system_is_pt() -> bool {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .find(|v| !v.is_empty())
        .is_some_and(|v| v.to_lowercase().starts_with("pt"))
}

/// `choice`: "auto", "en" or "pt". Must run after the first window exists (Slint).
pub fn apply(choice: &str) {
    let pt = match choice {
        "pt" => true,
        "en" => false,
        _ => system_is_pt(),
    };
    PT.store(pt, Ordering::Relaxed);
    let _ = slint::select_bundled_translation(if pt { "pt_BR" } else { "en" });
}

/// For the command line, where there is no window to load translations into.
pub fn apply_lang(choice: &str) {
    PT.store(choice == "pt" || (choice != "en" && system_is_pt()), Ordering::Relaxed);
}

/// `tr!("Installed: {name}", "Instalado: {name}")`: picks the text for the current
/// language and formats it like `format!`.
#[macro_export]
macro_rules! tr {
    ($en:literal, $pt:literal $(,)?) => {
        if $crate::i18n::is_pt() { format!($pt) } else { format!($en) }
    };
    ($en:literal, $pt:literal, $($arg:tt)*) => {
        if $crate::i18n::is_pt() { format!($pt, $($arg)*) } else { format!($en, $($arg)*) }
    };
}
