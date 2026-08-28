use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::NaiveDate;
use gtk::glib;
use photoscanner::default_output_directory;

const GROUP: &str = "settings";

#[derive(Clone, Debug, PartialEq)]
pub struct PersistedSettings {
    pub output_directory: PathBuf,
    pub dpi_index: u32,
    pub format_index: u32,
    pub mode_index: u32,
    pub quality: f64,
    pub min_area: f64,
    pub padding: f64,
    pub auto_threshold: bool,
    pub threshold: f64,
    pub review_before_save: bool,
    pub capture_date: Option<NaiveDate>,
}

impl Default for PersistedSettings {
    fn default() -> Self {
        Self {
            output_directory: default_output_directory(),
            dpi_index: 1,
            format_index: 0,
            mode_index: 0,
            quality: 95.0,
            min_area: 2.0,
            padding: 1.2,
            auto_threshold: true,
            threshold: 12.0,
            review_before_save: true,
            capture_date: None,
        }
    }
}

impl PersistedSettings {
    pub fn load(path: &Path) -> Self {
        let defaults = Self::default();
        let key_file = glib::KeyFile::new();
        if key_file
            .load_from_file(path, glib::KeyFileFlags::NONE)
            .is_err()
        {
            return defaults;
        }

        let output_directory = key_file
            .string(GROUP, "output_directory")
            .ok()
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| defaults.output_directory.clone());
        let capture_date = key_file
            .string(GROUP, "capture_date")
            .ok()
            .and_then(|value| NaiveDate::parse_from_str(&value, "%Y-%m-%d").ok());

        Self {
            output_directory,
            dpi_index: read_index(&key_file, "dpi_index", defaults.dpi_index, 2),
            format_index: read_index(&key_file, "format_index", defaults.format_index, 2),
            mode_index: read_index(&key_file, "mode_index", defaults.mode_index, 1),
            quality: read_double(&key_file, "quality", defaults.quality, 1.0, 100.0),
            min_area: read_double(&key_file, "min_area", defaults.min_area, 0.1, 50.0),
            padding: read_double(&key_file, "padding", defaults.padding, 0.0, 15.0),
            auto_threshold: key_file
                .boolean(GROUP, "auto_threshold")
                .unwrap_or(defaults.auto_threshold),
            threshold: read_double(&key_file, "threshold", defaults.threshold, 1.0, 255.0),
            review_before_save: key_file
                .boolean(GROUP, "review_before_save")
                .unwrap_or(defaults.review_before_save),
            capture_date,
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "Einstellungsordner konnte nicht angelegt werden: {}",
                    parent.display()
                )
            })?;
        }
        let key_file = glib::KeyFile::new();
        key_file.set_string(
            GROUP,
            "output_directory",
            &self.output_directory.to_string_lossy(),
        );
        key_file.set_integer(GROUP, "dpi_index", self.dpi_index as i32);
        key_file.set_integer(GROUP, "format_index", self.format_index as i32);
        key_file.set_integer(GROUP, "mode_index", self.mode_index as i32);
        key_file.set_double(GROUP, "quality", self.quality);
        key_file.set_double(GROUP, "min_area", self.min_area);
        key_file.set_double(GROUP, "padding", self.padding);
        key_file.set_boolean(GROUP, "auto_threshold", self.auto_threshold);
        key_file.set_double(GROUP, "threshold", self.threshold);
        key_file.set_boolean(GROUP, "review_before_save", self.review_before_save);
        if let Some(date) = self.capture_date {
            key_file.set_string(GROUP, "capture_date", &date.format("%Y-%m-%d").to_string());
        }
        key_file.save_to_file(path).with_context(|| {
            format!(
                "Einstellungen konnten nicht gespeichert werden: {}",
                path.display()
            )
        })
    }
}

fn read_index(key_file: &glib::KeyFile, key: &str, default: u32, maximum: u32) -> u32 {
    key_file
        .integer(GROUP, key)
        .ok()
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(default)
        .min(maximum)
}

fn read_double(key_file: &glib::KeyFile, key: &str, default: f64, min: f64, max: f64) -> f64 {
    key_file
        .double(GROUP, key)
        .ok()
        .filter(|value| value.is_finite())
        .unwrap_or(default)
        .clamp(min, max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn settings_round_trip() {
        let directory = TempDir::new().expect("temp dir");
        let path = directory.path().join("settings.ini");
        let expected = PersistedSettings {
            output_directory: directory.path().join("photos"),
            dpi_index: 2,
            format_index: 1,
            mode_index: 1,
            quality: 82.0,
            min_area: 4.5,
            padding: 2.4,
            auto_threshold: false,
            threshold: 42.0,
            review_before_save: false,
            capture_date: NaiveDate::from_ymd_opt(1995, 9, 1),
        };

        expected.save(&path).expect("save settings");

        assert_eq!(PersistedSettings::load(&path), expected);
    }

    #[test]
    fn loading_clamps_widget_values() {
        let directory = TempDir::new().expect("temp dir");
        let path = directory.path().join("settings.ini");
        let invalid = PersistedSettings {
            dpi_index: 99,
            format_index: 99,
            mode_index: 99,
            quality: 1000.0,
            min_area: -5.0,
            padding: 99.0,
            threshold: 0.0,
            ..PersistedSettings::default()
        };
        invalid.save(&path).expect("save settings");

        let loaded = PersistedSettings::load(&path);

        assert_eq!(loaded.dpi_index, 2);
        assert_eq!(loaded.format_index, 2);
        assert_eq!(loaded.mode_index, 1);
        assert_eq!(loaded.quality, 100.0);
        assert_eq!(loaded.min_area, 0.1);
        assert_eq!(loaded.padding, 15.0);
        assert_eq!(loaded.threshold, 1.0);
    }

    #[test]
    fn corrupt_file_uses_defaults() {
        let directory = TempDir::new().expect("temp dir");
        let path = directory.path().join("settings.ini");
        fs::write(&path, "not a key file").expect("write corrupt settings");

        assert_eq!(PersistedSettings::load(&path), PersistedSettings::default());
    }
}
