//! Scannerzugriff und Bildaufteilung für Photo Scanner.
//!
//! Die Bibliothek kapselt die SANE-Prozesssteuerung sowie die sichere,
//! kollisionsfreie Verarbeitung von PNG-, JPEG- und TIFF-Scans.

#![warn(missing_docs)]

use std::path::PathBuf;

mod metadata;

/// SANE-Geräteerkennung und abbrechbare Scanprozesse.
pub mod scanner;
/// Fotoerkennung, Begradigung und Metadatenexport.
pub mod splitter;

/// Eindeutige Anwendungs-ID für GTK, Desktop-Datei und AppStream.
pub const APP_ID: &str = "de.martin.PhotoScanner";
/// Anzeigename der Anwendung.
pub const APP_NAME: &str = "Photo Scanner";

/// Liefert den gemeinsamen Standard-Ausgabeordner für GUI und CLI.
pub fn default_output_directory() -> PathBuf {
    dirs::picture_dir()
        .unwrap_or_else(|| PathBuf::from("output"))
        .join("PhotoScanner")
}
