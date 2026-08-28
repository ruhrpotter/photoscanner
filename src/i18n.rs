//! Gettext-Initialisierung und kleine Formatierhilfen.

use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use gettextrs::{
    LocaleCategory, bind_textdomain_codeset, bindtextdomain, gettext, ngettext, setlocale,
    textdomain,
};

const DOMAIN: &str = "photoscanner";

/// Initialisiert die Prozess-Locale und bindet den Photo-Scanner-Textkatalog.
pub fn initialize() -> Result<(), io::Error> {
    // SAFETY: This runs once at process start, before worker threads are created.
    unsafe {
        setlocale(LocaleCategory::LcAll, "");
    }
    bindtextdomain(DOMAIN, locale_directory())?;
    bind_textdomain_codeset(DOMAIN, "UTF-8")?;
    textdomain(DOMAIN)?;
    Ok(())
}

/// Liefert den aktiven Locale-Ordner für Benutzer- oder Systeminstallation.
pub fn locale_directory() -> PathBuf {
    if let Some(override_path) = std::env::var_os("PHOTOSCANNER_LOCALE_DIR") {
        return PathBuf::from(override_path);
    }
    let user = dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from(".local/share"))
        .join("locale");
    if contains_catalog(&user) {
        user
    } else {
        PathBuf::from("/usr/share/locale")
    }
}

fn contains_catalog(locale_root: &Path) -> bool {
    fs::read_dir(locale_root)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .any(|entry| {
            entry
                .path()
                .join("LC_MESSAGES")
                .join(format!("{DOMAIN}.mo"))
                .is_file()
        })
}

/// Übersetzt einen unveränderlichen englischen msgid.
pub fn tr(message: &str) -> String {
    gettext(message)
}

/// Übersetzt einen msgid und ersetzt benannte Platzhalter.
pub fn tr_args(message: &str, arguments: &[(&str, String)]) -> String {
    let mut translated = gettext(message);
    for (name, value) in arguments {
        translated = translated.replace(&format!("{{{name}}}"), value);
    }
    translated
}

/// Übersetzt Singular oder Plural und ersetzt den Platzhalter `{count}`.
pub fn trn(singular: &str, plural: &str, count: usize) -> String {
    ngettext(singular, plural, count as u32).replace("{count}", &count.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_detection_requires_the_domain_file() {
        let directory = tempfile::tempdir().unwrap();
        assert!(!contains_catalog(directory.path()));
        let messages = directory.path().join("de/LC_MESSAGES");
        fs::create_dir_all(&messages).unwrap();
        fs::write(messages.join("photoscanner.mo"), []).unwrap();
        assert!(contains_catalog(directory.path()));
    }
}
