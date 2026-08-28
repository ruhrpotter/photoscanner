use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use chrono::{DateTime, Local};
use filetime::{FileTime, set_file_mtime};

use crate::i18n::{tr, tr_args};

#[derive(Debug)]
pub(crate) enum MetadataError {
    ToolMissing,
    Process(io::Error),
    Read { path: PathBuf, detail: String },
    Write { path: PathBuf, detail: String },
    Path(io::Error),
    FileTime(io::Error),
}

impl std::fmt::Display for MetadataError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::ToolMissing => tr(
                "The 'exiv2' image metadata program was not found. Install it with your package manager (e.g. 'sudo pacman -S exiv2' on Arch).",
            ),
            Self::Process(error) => tr_args(
                "The metadata process could not be executed: {error}",
                &[("error", error.to_string())],
            ),
            Self::Read { path, detail } => tr_args(
                "Metadata from '{path}' could not be read: {detail}",
                &[
                    ("path", path.display().to_string()),
                    ("detail", detail.clone()),
                ],
            ),
            Self::Write { path, detail } => tr_args(
                "Metadata for '{path}' could not be written: {detail}",
                &[
                    ("path", path.display().to_string()),
                    ("detail", detail.clone()),
                ],
            ),
            Self::Path(error) => tr_args(
                "The metadata file path could not be resolved: {error}",
                &[("error", error.to_string())],
            ),
            Self::FileTime(error) => tr_args(
                "The modification date could not be set: {error}",
                &[("error", error.to_string())],
            ),
        };
        formatter.write_str(&message)
    }
}

impl std::error::Error for MetadataError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Process(error) | Self::Path(error) | Self::FileTime(error) => Some(error),
            _ => None,
        }
    }
}

fn command_output(command: &mut Command) -> Result<Output, MetadataError> {
    command.output().map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            MetadataError::ToolMissing
        } else {
            MetadataError::Process(error)
        }
    })
}

fn canonical_path(path: &Path) -> Result<PathBuf, MetadataError> {
    fs::canonicalize(path).map_err(MetadataError::Path)
}

fn detail(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        tr_args(
            "exiv2 exited with status {status}",
            &[("status", output.status.to_string())],
        )
    } else {
        stderr
    }
}

pub(crate) fn read_tag(path: &Path, tag: &str) -> Result<Option<String>, MetadataError> {
    let path = canonical_path(path)?;
    let output = command_output(Command::new("exiv2").args(["-K", tag, "-Pv"]).arg(&path))?;
    if !output.status.success() {
        if output.status.code() == Some(1) && output.stdout.is_empty() && output.stderr.is_empty() {
            return Ok(None);
        }
        return Err(MetadataError::Read {
            path,
            detail: detail(&output),
        });
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok((!value.is_empty()).then_some(value))
}

fn parse_positive_rational(value: &str) -> Option<u32> {
    let (numerator, denominator) = value.split_once('/').unwrap_or((value, "1"));
    let numerator = numerator.trim().parse::<f64>().ok()?;
    let denominator = denominator.trim().parse::<f64>().ok()?;
    if !numerator.is_finite() || !denominator.is_finite() || denominator <= 0.0 {
        return None;
    }
    let result = (numerator / denominator).round();
    (result.is_finite() && result > 0.0 && result <= u32::MAX as f64).then_some(result as u32)
}

pub(crate) fn image_orientation(path: &Path) -> Result<Option<u16>, MetadataError> {
    Ok(read_tag(path, "Exif.Image.Orientation")?
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|value| (1..=8).contains(value)))
}

pub(crate) fn image_dpi(path: &Path) -> Result<Option<u32>, MetadataError> {
    Ok(read_tag(path, "Exif.Image.XResolution")?
        .as_deref()
        .and_then(parse_positive_rational))
}

pub(crate) fn write_metadata(
    path: &Path,
    captured_at: DateTime<Local>,
    dpi: Option<u32>,
) -> Result<(), MetadataError> {
    let path = canonical_path(path)?;
    let timestamp = captured_at.format("%Y:%m:%d %H:%M:%S").to_string();
    let offset = captured_at.format("%:z").to_string();
    let mut edits = vec![
        format!("set Exif.Image.DateTime Ascii {timestamp}"),
        format!("set Exif.Photo.DateTimeOriginal Ascii {timestamp}"),
        format!("set Exif.Photo.DateTimeDigitized Ascii {timestamp}"),
        "set Exif.Image.Software Ascii Photo Scanner".to_string(),
        format!("set Exif.Photo.OffsetTime Ascii {offset}"),
        format!("set Exif.Photo.OffsetTimeOriginal Ascii {offset}"),
        format!("set Exif.Photo.OffsetTimeDigitized Ascii {offset}"),
    ];
    if let Some(dpi) = dpi {
        edits.extend([
            format!("set Exif.Image.XResolution Rational {dpi}/1"),
            format!("set Exif.Image.YResolution Rational {dpi}/1"),
            "set Exif.Image.ResolutionUnit Short 2".to_string(),
        ]);
    }

    let mut command = Command::new("exiv2");
    command.args(["-q", "-k"]);
    for edit in edits {
        command.arg("-M").arg(edit);
    }
    let output = command.arg(&path).output().map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            MetadataError::ToolMissing
        } else {
            MetadataError::Process(error)
        }
    })?;
    if !output.status.success() {
        return Err(MetadataError::Write {
            path,
            detail: detail(&output),
        });
    }

    set_file_mtime(
        &path,
        FileTime::from_unix_time(
            captured_at.timestamp(),
            captured_at.timestamp_subsec_nanos(),
        ),
    )
    .map_err(MetadataError::FileTime)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_integer_and_fractional_resolution() {
        assert_eq!(parse_positive_rational("600"), Some(600));
        assert_eq!(parse_positive_rational("1200/2"), Some(600));
        assert_eq!(parse_positive_rational("1/0"), None);
        assert_eq!(parse_positive_rational("NaN"), None);
    }
}
