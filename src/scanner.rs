use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use thiserror::Error;
use wait_timeout::ChildExt;

#[derive(Clone, Debug)]
struct ScannerProgram {
    executable: OsString,
    prefix_arguments: Vec<OsString>,
}

impl ScannerProgram {
    fn new(executable: impl Into<OsString>) -> Self {
        Self {
            executable: executable.into(),
            prefix_arguments: Vec::new(),
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.executable);
        command.args(&self.prefix_arguments);
        command
    }
}

#[derive(Debug, Error)]
/// Fehler beim Erkennen von Scannern oder beim Ausführen eines Scans.
pub enum ScannerError {
    /// Das Programm `scanimage` ist nicht installiert oder nicht auffindbar.
    #[error(
        "SANE ist nicht installiert: Das Programm 'scanimage' wurde nicht gefunden. Unter CachyOS/Arch kann es mit 'sudo pacman -S sane sane-airscan' installiert werden."
    )]
    ScanimageMissing,
    /// Die Gerätesuche hat ihr Zeitlimit überschritten.
    #[error("Die Scannersuche hat zu lange gedauert.")]
    DeviceTimeout,
    /// SANE hat die Gerätesuche mit einer Fehlermeldung beendet.
    #[error("Scanner konnten nicht abgefragt werden: {0}")]
    DeviceQuery(String),
    /// Die angeforderte Auflösung liegt außerhalb des zulässigen Bereichs.
    #[error("Die Auflösung muss zwischen 75 und 2400 dpi liegen.")]
    InvalidDpi,
    /// Der Scan hat sein Zeitlimit überschritten.
    #[error("Der Scan hat das Zeitlimit überschritten.")]
    ScanTimeout,
    /// Der aufrufende Thread hat den laufenden Scannerprozess abgebrochen.
    #[error("Der Vorgang wurde abgebrochen.")]
    Cancelled,
    /// `scanimage` hat den Scan mit einer Fehlermeldung beendet.
    #[error("Der Scan ist fehlgeschlagen: {0}")]
    ScanFailed(String),
    /// `scanimage` wurde erfolgreich beendet, hat aber keine Daten geliefert.
    #[error("Der Scanner hat keine Bilddaten geliefert.")]
    EmptyScan,
    /// Die atomare Veröffentlichung wurde abgelehnt, weil das Ziel existiert.
    #[error("Die Zieldatei existiert bereits: {}", .0.display())]
    DestinationExists(PathBuf),
    /// Der Scannerprozess oder eine zugehörige Dateioperation ist fehlgeschlagen.
    #[error("Scannerprozess konnte nicht gestartet werden: {0}")]
    Process(#[from] std::io::Error),
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Ein von SANE gemeldetes Scannergerät.
pub struct ScannerDevice {
    /// Technischer SANE-Gerätename, der an `scanimage` übergeben wird.
    pub name: String,
    /// Herstellername aus der SANE-Geräteliste.
    pub vendor: String,
    /// Modellname aus der SANE-Geräteliste.
    pub model: String,
    /// Von SANE gemeldeter Gerätetyp.
    pub kind: String,
}

/// Ein einmal verwendbares, threadsicheres Abbruchsignal für Scannerprozesse.
///
/// Klone teilen sich denselben Zustand. Sobald ein Klon [`cancel`](Self::cancel)
/// aufruft, beenden die abbrechbaren Scannerfunktionen ihren Kindprozess und
/// räumen temporäre Dateien auf.
#[derive(Clone, Debug, Default)]
pub struct ScannerCancellation {
    cancelled: Arc<AtomicBool>,
}

impl ScannerCancellation {
    /// Erstellt ein neues, noch nicht ausgelöstes Abbruchsignal.
    pub fn new() -> Self {
        Self::default()
    }

    /// Löst das Signal dauerhaft für alle Klone aus.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Gibt zurück, ob das Signal bereits ausgelöst wurde.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

impl ScannerDevice {
    /// Liefert einen kompakten, menschenlesbaren Gerätenamen.
    pub fn display_name(&self) -> String {
        let vendor = self.vendor.trim();
        let model = self.model.trim();
        let vendor_is_protocol = ["escl", "wsd", "airscan"]
            .iter()
            .any(|protocol| vendor.eq_ignore_ascii_case(protocol));

        match (vendor_is_protocol || vendor.is_empty(), model.is_empty()) {
            (_, true) => self.name.clone(),
            (true, false) => model.to_string(),
            (false, false) => format!("{vendor} {model}"),
        }
    }

    /// Liefert einen ausführlichen Namen einschließlich SANE-Gerätename.
    pub fn label(&self) -> String {
        let description = format!("{} {}", self.vendor, self.model).trim().to_string();
        if description.is_empty() {
            format!("{} ({})", self.name, self.name)
        } else {
            format!("{description} ({})", self.name)
        }
    }
}

/// Prüft, ob `scanimage` auffindbar und tatsächlich ausführbar ist.
pub fn scanimage_available() -> bool {
    scanimage_available_at(&ScannerProgram::new("scanimage"))
}

fn scanimage_available_at(program: &ScannerProgram) -> bool {
    let Ok(mut child) = program
        .command()
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };

    match child.wait_timeout(Duration::from_secs(2)) {
        Ok(Some(_)) => true,
        Ok(None) => {
            let _ = terminate_and_reap(&mut child);
            true
        }
        Err(_) => {
            let _ = terminate_and_reap(&mut child);
            false
        }
    }
}

fn spawn_scanner(command: &mut Command) -> Result<Child, ScannerError> {
    command.spawn().map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            ScannerError::ScanimageMissing
        } else {
            ScannerError::Process(error)
        }
    })
}

enum WaitResult {
    Exited(std::process::ExitStatus),
    TimedOut,
    Cancelled,
}

fn terminate_and_reap(child: &mut Child) -> io::Result<()> {
    match child.try_wait() {
        Ok(Some(_)) => Ok(()),
        Ok(None) | Err(_) => {
            // Auch nach einem try_wait-Fehler versuchen wir, den eigenen
            // Kindprozess sicher zu beenden und einzusammeln.
            let _ = child.kill();
            child.wait().map(|_| ())
        }
    }
}

fn wait_for_child_cancellable(
    child: &mut Child,
    timeout: Duration,
    cancellation: Option<&ScannerCancellation>,
) -> io::Result<WaitResult> {
    let started = Instant::now();
    const POLL_INTERVAL: Duration = Duration::from_millis(20);

    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(WaitResult::Exited(status));
        }
        if cancellation.is_some_and(ScannerCancellation::is_cancelled) {
            terminate_and_reap(child)?;
            return Ok(WaitResult::Cancelled);
        }
        if started.elapsed() >= timeout {
            terminate_and_reap(child)?;
            return Ok(WaitResult::TimedOut);
        }

        thread::sleep(POLL_INTERVAL.min(timeout.saturating_sub(started.elapsed())));
    }
}

fn drain_capped<R: Read>(mut reader: R, limit: usize) -> io::Result<Vec<u8>> {
    let mut captured = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let remaining = limit.saturating_sub(captured.len());
        captured.extend_from_slice(&buffer[..count.min(remaining)]);
    }
    Ok(captured)
}

fn pipe_reader<R: Read + Send + 'static>(
    reader: R,
    limit: usize,
) -> JoinHandle<io::Result<Vec<u8>>> {
    thread::spawn(move || drain_capped(reader, limit))
}

fn join_reader(reader: JoinHandle<io::Result<Vec<u8>>>) -> Result<Vec<u8>, ScannerError> {
    reader
        .join()
        .map_err(|_| ScannerError::Process(io::Error::other("Pipe-Lesethread abgebrochen")))?
        .map_err(ScannerError::Process)
}

fn wait_for_output(
    mut child: Child,
    timeout: Duration,
    cancellation: Option<&ScannerCancellation>,
) -> Result<std::process::Output, ScannerError> {
    const STDOUT_LIMIT: usize = 1024 * 1024;
    const STDERR_LIMIT: usize = 256 * 1024;

    let stdout_reader = child
        .stdout
        .take()
        .map(|pipe| pipe_reader(pipe, STDOUT_LIMIT));
    let stderr_reader = child
        .stderr
        .take()
        .map(|pipe| pipe_reader(pipe, STDERR_LIMIT));

    let wait_result = wait_for_child_cancellable(&mut child, timeout, cancellation);
    if wait_result.is_err() {
        let _ = terminate_and_reap(&mut child);
    }
    let stdout = stdout_reader
        .map(join_reader)
        .transpose()?
        .unwrap_or_default();
    let stderr = stderr_reader
        .map(join_reader)
        .transpose()?
        .unwrap_or_default();

    let status = match wait_result? {
        WaitResult::Exited(status) => status,
        WaitResult::TimedOut => return Err(ScannerError::DeviceTimeout),
        WaitResult::Cancelled => return Err(ScannerError::Cancelled),
    };
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

/// Wandelt die tabulatorgetrennte Ausgabe von `scanimage -f` in Geräte um.
///
/// Video4Linux-Geräte werden herausgefiltert, da sie keine Flachbettscanner
/// für diesen Arbeitsablauf darstellen.
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

/// Fragt verfügbare Scanner mit einem festen Zeitlimit ab.
pub fn list_devices(timeout: Duration) -> Result<Vec<ScannerDevice>, ScannerError> {
    list_devices_with_program(&ScannerProgram::new("scanimage"), timeout, None)
}

/// Fragt verfügbare Scanner ab und beendet den Prozess bei einem Abbruchsignal.
///
/// Das Abbruchsignal ist einmal verwendbar. Bereits abgebrochene Signale
/// verhindern, dass scanimage überhaupt gestartet wird.
pub fn list_devices_cancellable(
    timeout: Duration,
    cancellation: &ScannerCancellation,
) -> Result<Vec<ScannerDevice>, ScannerError> {
    if cancellation.is_cancelled() {
        return Err(ScannerError::Cancelled);
    }
    list_devices_with_program(
        &ScannerProgram::new("scanimage"),
        timeout,
        Some(cancellation),
    )
}

fn list_devices_with_program(
    program: &ScannerProgram,
    timeout: Duration,
    cancellation: Option<&ScannerCancellation>,
) -> Result<Vec<ScannerDevice>, ScannerError> {
    let child = spawn_scanner(
        program
            .command()
            .args(["-f", "%d\t%v\t%m\t%t%n"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )?;
    let result = wait_for_output(child, timeout, cancellation)?;
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

/// Scannt ohne externes Abbruchsignal in eine neue Zieldatei.
///
/// Die Zieldatei wird atomar veröffentlicht und niemals überschrieben.
pub fn scan_to_file(
    destination: &Path,
    device: Option<&str>,
    dpi: u32,
    timeout: Duration,
) -> Result<PathBuf, ScannerError> {
    scan_to_file_cancellable(
        destination,
        device,
        dpi,
        timeout,
        &ScannerCancellation::new(),
    )
}

/// Scannt in eine Datei und beendet den Scannerprozess bei einem Abbruchsignal.
///
/// Der Scan wird zunächst in eine eindeutige temporäre Datei im Zielordner
/// geschrieben. Die Zieldatei wird anschließend atomar veröffentlicht und
/// niemals überschrieben. Temporäre Dateien werden bei Fehler, Timeout oder
/// Abbruch automatisch entfernt.
pub fn scan_to_file_cancellable(
    destination: &Path,
    device: Option<&str>,
    dpi: u32,
    timeout: Duration,
    cancellation: &ScannerCancellation,
) -> Result<PathBuf, ScannerError> {
    scan_to_file_with_program(
        &ScannerProgram::new("scanimage"),
        destination,
        device,
        dpi,
        timeout,
        cancellation,
    )
}

fn scan_to_file_with_program(
    program: &ScannerProgram,
    destination: &Path,
    device: Option<&str>,
    dpi: u32,
    timeout: Duration,
    cancellation: &ScannerCancellation,
) -> Result<PathBuf, ScannerError> {
    if cancellation.is_cancelled() {
        return Err(ScannerError::Cancelled);
    }
    if !(75..=2400).contains(&dpi) {
        return Err(ScannerError::InvalidDpi);
    }
    let destination = destination.to_path_buf();
    let output_directory = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(output_directory)?;
    let temporary = tempfile::Builder::new()
        .prefix(".photoscanner-scan-")
        .suffix(".part")
        .tempfile_in(output_directory)?;
    let output = temporary.reopen()?;
    let mut command = program.command();
    if let Some(device) = device {
        command.args(["--device-name", device]);
    }
    let mut child = spawn_scanner(
        command
            .args([
                "--mode",
                "Color",
                "--resolution",
                &dpi.to_string(),
                "--format",
                "png",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::from(output))
            .stderr(Stdio::piped()),
    )?;

    const STDERR_LIMIT: usize = 256 * 1024;
    let stderr_reader = child
        .stderr
        .take()
        .map(|pipe| pipe_reader(pipe, STDERR_LIMIT));
    let wait_result = wait_for_child_cancellable(&mut child, timeout, Some(cancellation));
    if wait_result.is_err() {
        let _ = terminate_and_reap(&mut child);
    }
    let stderr = stderr_reader
        .map(join_reader)
        .transpose()?
        .unwrap_or_default();
    let status = match wait_result? {
        WaitResult::Exited(status) => status,
        WaitResult::TimedOut => return Err(ScannerError::ScanTimeout),
        WaitResult::Cancelled => return Err(ScannerError::Cancelled),
    };
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr);
        let detail = stderr.trim();
        return Err(ScannerError::ScanFailed(if detail.is_empty() {
            "unbekannter Fehler".to_string()
        } else {
            detail.to_string()
        }));
    }
    if temporary.as_file().metadata()?.len() == 0 {
        return Err(ScannerError::EmptyScan);
    }
    temporary.as_file().sync_all()?;
    match temporary.persist_noclobber(&destination) {
        Ok(_) => {}
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
            return Err(ScannerError::DestinationExists(destination));
        }
        Err(error) => return Err(ScannerError::Process(error.error)),
    }
    Ok(destination)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::io::Write as _;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[cfg(unix)]
    fn fake_scanimage(body: &str) -> (tempfile::TempDir, ScannerProgram) {
        let directory = tempfile::tempdir().expect("Temporärordner anlegen");
        let script = directory.path().join("scanimage.sh");
        let mut file = fs::File::create(&script).expect("Fake-scanimage anlegen");
        file.write_all(format!("#!/bin/sh\nset -eu\n{body}\n").as_bytes())
            .expect("Fake-scanimage schreiben");
        file.sync_all().expect("Fake-scanimage synchronisieren");
        drop(file);
        let mut permissions = fs::metadata(&script)
            .expect("Fake-scanimage lesen")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).expect("Fake-scanimage ausführbar machen");
        (
            directory,
            ScannerProgram {
                executable: OsString::from("/bin/sh"),
                prefix_arguments: vec![script.into_os_string()],
            },
        )
    }

    fn scan_temp_files(directory: &Path) -> Vec<PathBuf> {
        fs::read_dir(directory)
            .expect("Ausgabeordner lesen")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name().is_some_and(|name| {
                    let name = name.to_string_lossy();
                    name.starts_with(".photoscanner-scan-") && name.ends_with(".part")
                })
            })
            .collect()
    }

    fn assert_no_scan_temp_files(directory: &Path) {
        assert!(
            scan_temp_files(directory).is_empty(),
            "Temporäre Scandateien wurden nicht aufgeräumt"
        );
    }

    #[test]
    fn parses_devices_and_filters_webcams() {
        let devices = parse_devices(
            "airscan:e0:Scanner\tEpson\tET-4850\tflatbed\n\
             v4l:/dev/video0\tGeneric\tWebcam\tvideo\n",
        );
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].name, "airscan:e0:Scanner");
        assert_eq!(devices[0].label(), "Epson ET-4850 (airscan:e0:Scanner)");
        assert_eq!(devices[0].display_name(), "Epson ET-4850");
    }

    #[test]
    fn omits_protocol_vendor_from_display_name() {
        let device = ScannerDevice {
            name: "airscan:e0:Brother MFC-L2960DW".to_string(),
            vendor: "eSCL".to_string(),
            model: "Brother MFC-L2960DW".to_string(),
            kind: "ip=192.0.2.1".to_string(),
        };

        assert_eq!(device.display_name(), "Brother MFC-L2960DW");
        assert_eq!(
            device.label(),
            "eSCL Brother MFC-L2960DW (airscan:e0:Brother MFC-L2960DW)"
        );
    }

    #[cfg(unix)]
    #[test]
    fn drains_large_device_stdout_and_stderr_without_deadlock() {
        let (_directory, program) = fake_scanimage(
            r#"
i=0
while [ "$i" -lt 12000 ]; do
    printf 'test:device-%s\tVendor\tModel\tflatbed\n' "$i"
    printf 'Sehr lange SANE-Diagnosezeile, die die Kapazität einer Pipe deutlich überschreitet: %s........................................\n' "$i" >&2
    i=$((i + 1))
done
"#,
        );

        let devices = list_devices_with_program(&program, Duration::from_secs(5), None)
            .expect("Geräteabfrage darf nicht an einer vollen Pipe hängen");

        assert!(!devices.is_empty());
        assert_eq!(devices[0].name, "test:device-0");
    }

    #[cfg(unix)]
    #[test]
    fn device_query_timeout_and_cancellation_reap_processes() {
        let output_directory = tempfile::tempdir().expect("Ausgabeordner anlegen");
        let pid_file = output_directory.path().join("device-query.pid");
        let script = format!(
            r#"
printf '%s' "$$" > '{}'
while :; do :; done
"#,
            pid_file.display()
        );
        let (_directory, program) = fake_scanimage(&script);

        let error = list_devices_with_program(&program, Duration::from_millis(80), None)
            .expect_err("Endlose Geräteabfrage muss in den Timeout laufen");
        assert!(matches!(error, ScannerError::DeviceTimeout));
        #[cfg(target_os = "linux")]
        {
            let pid = fs::read_to_string(&pid_file).expect("Prozess-ID lesen");
            assert!(!Path::new("/proc").join(pid).exists());
        }

        let cancellation = ScannerCancellation::new();
        let cancellation_trigger = cancellation.clone();
        let trigger = thread::spawn(move || {
            thread::sleep(Duration::from_millis(80));
            cancellation_trigger.cancel();
        });
        let error =
            list_devices_with_program(&program, Duration::from_secs(5), Some(&cancellation))
                .expect_err("Abbruchsignal muss die Geräteabfrage beenden");
        trigger.join().expect("Abbruchthread beenden");
        assert!(matches!(error, ScannerError::Cancelled));
        #[cfg(target_os = "linux")]
        {
            let pid = fs::read_to_string(&pid_file).expect("Prozess-ID lesen");
            assert!(!Path::new("/proc").join(pid).exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn drains_large_scan_stderr_without_deadlock() {
        let (_directory, program) = fake_scanimage(
            r#"
i=0
while [ "$i" -lt 12000 ]; do
    printf 'Sehr lange Scan-Diagnosezeile, die die Kapazität einer Pipe deutlich überschreitet: %s........................................\n' "$i" >&2
    i=$((i + 1))
done
printf 'fake-png-data'
"#,
        );
        let output_directory = tempfile::tempdir().expect("Ausgabeordner anlegen");
        let destination = output_directory.path().join("scan.png");

        let result = scan_to_file_with_program(
            &program,
            &destination,
            None,
            300,
            Duration::from_secs(5),
            &ScannerCancellation::new(),
        )
        .expect("Scan darf nicht an einer vollen stderr-Pipe hängen");

        assert_eq!(result, destination);
        assert_eq!(fs::read(&result).expect("Scan lesen"), b"fake-png-data");
        assert_no_scan_temp_files(output_directory.path());
    }

    #[cfg(unix)]
    #[test]
    fn times_out_reaps_process_and_removes_partial_file() {
        let output_directory = tempfile::tempdir().expect("Ausgabeordner anlegen");
        let pid_file = output_directory.path().join("timeout.pid");
        let script = format!(
            r#"
printf '%s' "$$" > '{}'
printf 'partial-data'
while :; do :; done
"#,
            pid_file.display()
        );
        let (_directory, program) = fake_scanimage(&script);
        let destination = output_directory.path().join("scan.png");
        let started = Instant::now();

        let error = scan_to_file_with_program(
            &program,
            &destination,
            None,
            300,
            Duration::from_millis(80),
            &ScannerCancellation::new(),
        )
        .expect_err("Endloser Scannerprozess muss in den Timeout laufen");

        assert!(matches!(error, ScannerError::ScanTimeout));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(!destination.exists());
        assert_no_scan_temp_files(output_directory.path());
        #[cfg(target_os = "linux")]
        {
            let pid = fs::read_to_string(&pid_file).expect("Prozess-ID lesen");
            assert!(
                !Path::new("/proc").join(pid).exists(),
                "Scannerprozess wurde nicht eingesammelt"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_reaps_process_and_removes_partial_file() {
        let output_directory = tempfile::tempdir().expect("Ausgabeordner anlegen");
        let pid_file = output_directory.path().join("cancel.pid");
        let script = format!(
            r#"
printf '%s' "$$" > '{}'
printf 'partial-data'
while :; do :; done
"#,
            pid_file.display()
        );
        let (_directory, program) = fake_scanimage(&script);
        let destination = output_directory.path().join("scan.png");
        let cancellation = ScannerCancellation::new();
        let cancellation_trigger = cancellation.clone();
        let trigger = thread::spawn(move || {
            thread::sleep(Duration::from_millis(80));
            cancellation_trigger.cancel();
        });
        let started = Instant::now();

        let error = scan_to_file_with_program(
            &program,
            &destination,
            None,
            300,
            Duration::from_secs(5),
            &cancellation,
        )
        .expect_err("Abbruchsignal muss den Scannerprozess beenden");
        trigger.join().expect("Abbruchthread beenden");

        assert!(matches!(error, ScannerError::Cancelled));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(!destination.exists());
        assert_no_scan_temp_files(output_directory.path());
        #[cfg(target_os = "linux")]
        {
            let pid = fs::read_to_string(&pid_file).expect("Prozess-ID lesen");
            assert!(
                !Path::new("/proc").join(pid).exists(),
                "Scannerprozess wurde nicht eingesammelt"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn scan_failures_and_start_errors_remove_partial_file() {
        let (_directory, program) = fake_scanimage(
            r#"
printf 'partial-data'
printf 'absichtlicher Fehler\n' >&2
exit 7
"#,
        );
        let output_directory = tempfile::tempdir().expect("Ausgabeordner anlegen");
        let failed_destination = output_directory.path().join("failed.png");
        let error = scan_to_file_with_program(
            &program,
            &failed_destination,
            None,
            300,
            Duration::from_secs(2),
            &ScannerCancellation::new(),
        )
        .expect_err("Fehlerstatus muss gemeldet werden");
        assert!(matches!(error, ScannerError::ScanFailed(_)));
        assert_no_scan_temp_files(output_directory.path());

        let missing_destination = output_directory.path().join("missing.png");
        let error = scan_to_file_with_program(
            &ScannerProgram::new("/definitely/missing/fake-scanimage"),
            &missing_destination,
            None,
            300,
            Duration::from_secs(2),
            &ScannerCancellation::new(),
        )
        .expect_err("Startfehler muss gemeldet werden");
        assert!(matches!(error, ScannerError::ScanimageMissing));
        assert_no_scan_temp_files(output_directory.path());
    }

    #[cfg(unix)]
    #[test]
    fn existing_destination_is_never_overwritten() {
        let (_directory, program) = fake_scanimage("printf 'replacement-data'");
        let output_directory = tempfile::tempdir().expect("Ausgabeordner anlegen");
        let destination = output_directory.path().join("scan.png");
        fs::write(&destination, b"original-data").expect("Bestehendes Ziel schreiben");

        let error = scan_to_file_with_program(
            &program,
            &destination,
            None,
            300,
            Duration::from_secs(2),
            &ScannerCancellation::new(),
        )
        .expect_err("Bestehendes Ziel muss erhalten bleiben");

        assert!(matches!(error, ScannerError::DestinationExists(path) if path == destination));
        assert_eq!(
            fs::read(&destination).expect("Bestehendes Ziel lesen"),
            b"original-data"
        );
        assert_no_scan_temp_files(output_directory.path());
    }

    #[cfg(unix)]
    #[test]
    fn parallel_scans_use_unique_temporary_files_and_commit_without_clobbering() {
        let output_directory = tempfile::tempdir().expect("Ausgabeordner anlegen");
        let release = output_directory.path().join("release");
        let script = format!(
            r#"
while [ ! -f '{}' ]; do :; done
printf '%s' "$$"
"#,
            release.display()
        );
        let (_directory, program) = fake_scanimage(&script);
        let destination = output_directory.path().join("scan.png");

        let first_program = program.clone();
        let first_destination = destination.clone();
        let first = thread::spawn(move || {
            scan_to_file_with_program(
                &first_program,
                &first_destination,
                None,
                300,
                Duration::from_secs(5),
                &ScannerCancellation::new(),
            )
        });
        let second_program = program.clone();
        let second_destination = destination.clone();
        let second = thread::spawn(move || {
            scan_to_file_with_program(
                &second_program,
                &second_destination,
                None,
                300,
                Duration::from_secs(5),
                &ScannerCancellation::new(),
            )
        });

        let started = Instant::now();
        let temporary_files = loop {
            let files = scan_temp_files(output_directory.path());
            if files.len() >= 2 || started.elapsed() >= Duration::from_secs(2) {
                break files;
            }
            thread::sleep(Duration::from_millis(10));
        };
        fs::write(&release, []).expect("Scannerprozesse freigeben");

        let first = first.join().expect("Ersten Scanthread beenden");
        let second = second.join().expect("Zweiten Scanthread beenden");
        assert_eq!(
            temporary_files.len(),
            2,
            "Parallele Scans müssen zwei eigene Tempdateien verwenden"
        );
        assert_ne!(temporary_files[0], temporary_files[1]);
        assert!(
            matches!(
                (&first, &second),
                (Ok(path), Err(ScannerError::DestinationExists(existing)))
                    | (Err(ScannerError::DestinationExists(existing)), Ok(path))
                    if path == &destination && existing == &destination
            ),
            "Genau ein paralleler Scan darf das Ziel veröffentlichen"
        );
        assert!(!fs::read(&destination).expect("Scan lesen").is_empty());
        assert_no_scan_temp_files(output_directory.path());
    }

    #[cfg(unix)]
    #[test]
    fn availability_checks_actual_executability() {
        assert!(scanimage_available_at(&ScannerProgram::new("/bin/true")));

        let directory = tempfile::tempdir().expect("Temporärordner anlegen");
        let program = directory.path().join("nicht-ausführbar");
        fs::write(&program, b"#!/bin/sh\nexit 0\n").expect("Testprogramm schreiben");
        let mut permissions = fs::metadata(&program)
            .expect("Fake-scanimage lesen")
            .permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&program, permissions).expect("Ausführungsrecht entfernen");
        assert!(!scanimage_available_at(&ScannerProgram::new(
            program.into_os_string()
        )));
        assert!(!scanimage_available_at(&ScannerProgram::new(
            "/definitely/missing/fake-scanimage"
        )));
    }
}
