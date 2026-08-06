use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use thiserror::Error;
use wait_timeout::ChildExt;

#[derive(Debug, Error)]
pub enum ScannerError {
    #[error(
        "SANE ist nicht installiert: Das Programm 'scanimage' wurde nicht gefunden. Unter CachyOS/Arch kann es mit 'sudo pacman -S sane sane-airscan' installiert werden."
    )]
    ScanimageMissing,
    #[error("Die Scannersuche hat zu lange gedauert.")]
    DeviceTimeout,
    #[error("Scanner konnten nicht abgefragt werden: {0}")]
    DeviceQuery(String),
    #[error("Die Auflösung muss zwischen 75 und 2400 dpi liegen.")]
    InvalidDpi,
    #[error("Der Scan hat das Zeitlimit überschritten.")]
    ScanTimeout,
    #[error("Der Scan ist fehlgeschlagen: {0}")]
    ScanFailed(String),
    #[error("Der Scanner hat keine Bilddaten geliefert.")]
    EmptyScan,
    #[error("Scannerprozess konnte nicht gestartet werden: {0}")]
    Process(#[from] std::io::Error),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScannerDevice {
    pub name: String,
    pub vendor: String,
    pub model: String,
    pub kind: String,
}

impl ScannerDevice {
    pub fn label(&self) -> String {
        let description = format!("{} {}", self.vendor, self.model).trim().to_string();
        if description.is_empty() {
            format!("{} ({})", self.name, self.name)
        } else {
            format!("{description} ({})", self.name)
        }
    }
}

pub fn scanimage_available() -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|directory| directory.join("scanimage").is_file())
    })
}

fn wait_for_output(
    mut child: Child,
    timeout: Duration,
) -> Result<std::process::Output, ScannerError> {
    let status = match child.wait_timeout(timeout)? {
        Some(status) => status,
        None => {
            child.kill()?;
            child.wait()?;
            return Err(ScannerError::DeviceTimeout);
        }
    };
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if let Some(mut pipe) = child.stdout.take() {
        pipe.read_to_end(&mut stdout)?;
    }
    if let Some(mut pipe) = child.stderr.take() {
        pipe.read_to_end(&mut stderr)?;
    }
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

pub fn parse_devices(output: &str) -> Vec<ScannerDevice> {
    output
        .lines()
        .filter_map(|line| {
            if line.trim().is_empty() {
                return None;
            }
            let mut fields = line.split('\t');
            let device = ScannerDevice {
                name: fields.next().unwrap_or_default().trim().to_string(),
                vendor: fields.next().unwrap_or_default().trim().to_string(),
                model: fields.next().unwrap_or_default().trim().to_string(),
                kind: fields.next().unwrap_or_default().trim().to_string(),
            };
            (!device.name.starts_with("v4l:")).then_some(device)
        })
        .collect()
}

pub fn list_devices(timeout: Duration) -> Result<Vec<ScannerDevice>, ScannerError> {
    if !scanimage_available() {
        return Err(ScannerError::ScanimageMissing);
    }
    let child = Command::new("scanimage")
        .args(["-f", "%d\t%v\t%m\t%t%n"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let result = wait_for_output(child, timeout)?;
    if !result.status.success() {
        let detail = String::from_utf8_lossy(&result.stderr).trim().to_string();
        return Err(ScannerError::DeviceQuery(if detail.is_empty() {
            "unbekannter SANE-Fehler".to_string()
        } else {
            detail
        }));
    }
    Ok(parse_devices(&String::from_utf8_lossy(&result.stdout)))
}

fn partial_path(destination: &Path) -> PathBuf {
    let mut name = destination.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    destination.with_file_name(name)
}

pub fn scan_to_file(
    destination: &Path,
    device: Option<&str>,
    dpi: u32,
    timeout: Duration,
) -> Result<PathBuf, ScannerError> {
    if !scanimage_available() {
        return Err(ScannerError::ScanimageMissing);
    }
    if !(75..=2400).contains(&dpi) {
        return Err(ScannerError::InvalidDpi);
    }
    let destination = destination.to_path_buf();
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = partial_path(&destination);
    let output = File::create(&temporary)?;
    let mut command = Command::new("scanimage");
    if let Some(device) = device {
        command.args(["--device-name", device]);
    }
    let mut child = command
        .args([
            "--mode",
            "Color",
            "--resolution",
            &dpi.to_string(),
            "--format",
            "png",
        ])
        .stdout(Stdio::from(output))
        .stderr(Stdio::piped())
        .spawn()?;

    let status = match child.wait_timeout(timeout)? {
        Some(status) => status,
        None => {
            child.kill()?;
            child.wait()?;
            let _ = fs::remove_file(&temporary);
            return Err(ScannerError::ScanTimeout);
        }
    };
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        pipe.read_to_string(&mut stderr)?;
    }
    if !status.success() {
        let _ = fs::remove_file(&temporary);
        let detail = stderr.trim();
        return Err(ScannerError::ScanFailed(if detail.is_empty() {
            "unbekannter Fehler".to_string()
        } else {
            detail.to_string()
        }));
    }
    if fs::metadata(&temporary).map_or(true, |metadata| metadata.len() == 0) {
        let _ = fs::remove_file(&temporary);
        return Err(ScannerError::EmptyScan);
    }
    fs::rename(&temporary, &destination)?;
    Ok(destination)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_devices_and_filters_webcams() {
        let devices = parse_devices(
            "airscan:e0:Scanner\tEpson\tET-4850\tflatbed\n\
             v4l:/dev/video0\tGeneric\tWebcam\tvideo\n",
        );
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].name, "airscan:e0:Scanner");
        assert_eq!(devices[0].label(), "Epson ET-4850 (airscan:e0:Scanner)");
    }
}
