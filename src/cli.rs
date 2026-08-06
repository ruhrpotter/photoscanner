use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::NaiveDate;
use clap::{Args, Parser, Subcommand, ValueEnum};
use photoscanner::scanner::{list_devices, scan_to_file};
use photoscanner::splitter::{OutputFormat, SplitConfig, save_full_scan, split_scan};
use tempfile::TempDir;

#[derive(Debug, Parser)]
#[command(
    name = "photoscanner",
    version,
    about = "Papierfotos scannen, erkennen und einzeln speichern"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Native GTK4-Oberfläche öffnen
    Gui,
    /// Von SANE erkannte Scanner anzeigen
    Devices,
    /// Vorhandene Scandatei automatisch aufteilen
    Split {
        source: PathBuf,
        #[command(flatten)]
        split: SplitOptions,
        #[command(flatten)]
        export: ExportOptions,
    },
    /// Scannen und gefundene Papierfotos automatisch aufteilen
    Scan {
        #[command(flatten)]
        scanner: ScannerOptions,
        #[command(flatten)]
        split: SplitOptions,
        #[command(flatten)]
        export: ExportOptions,
    },
    /// Gesamte Scanfläche ohne Fotoanalyse speichern
    ScanFull {
        #[command(flatten)]
        scanner: ScannerOptions,
        #[command(flatten)]
        export: ExportOptions,
    },
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub enum FormatArgument {
    #[default]
    Jpg,
    Png,
    Tif,
}

impl From<FormatArgument> for OutputFormat {
    fn from(value: FormatArgument) -> Self {
        match value {
            FormatArgument::Jpg => Self::Jpeg,
            FormatArgument::Png => Self::Png,
            FormatArgument::Tif => Self::Tiff,
        }
    }
}

fn parse_date(value: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(value, "%d.%m.%Y")
        .map_err(|_| "Datum muss als TT.MM.JJJJ angegeben werden, z. B. 01.09.1995".to_string())
}

#[derive(Clone, Debug, Args)]
pub struct ExportOptions {
    #[arg(short, long, default_value = "output")]
    output: PathBuf,
    #[arg(long, value_enum, default_value_t)]
    format: FormatArgument,
    #[arg(long, default_value_t = 95, value_parser = clap::value_parser!(i32).range(1..=100))]
    quality: i32,
    #[arg(long)]
    prefix: Option<String>,
    #[arg(long, value_parser = parse_date, value_name = "TT.MM.JJJJ")]
    date: Option<NaiveDate>,
}

#[derive(Clone, Debug, Args)]
pub struct SplitOptions {
    #[arg(long, default_value_t = 2.0)]
    min_area: f64,
    #[arg(long)]
    threshold: Option<f64>,
    #[arg(long, default_value_t = 1.2)]
    padding: f64,
    #[arg(long)]
    no_preview: bool,
}

#[derive(Clone, Debug, Args)]
pub struct ScannerOptions {
    #[arg(long)]
    device: Option<String>,
    #[arg(long, default_value_t = 600, value_parser = clap::value_parser!(u32).range(75..=2400))]
    dpi: u32,
}

fn config(export: &ExportOptions, split: Option<&SplitOptions>, dpi: Option<u32>) -> SplitConfig {
    SplitConfig {
        min_area_percent: split.map_or(2.0, |options| options.min_area),
        threshold: split.and_then(|options| options.threshold),
        padding_percent: split.map_or(1.2, |options| options.padding),
        output_format: export.format.into(),
        jpeg_quality: export.quality,
        dpi,
        capture_date: export.date,
        ..SplitConfig::default()
    }
}

fn scanner_name(options: &ScannerOptions) -> Result<String> {
    if let Some(device) = &options.device {
        return Ok(device.clone());
    }
    let devices = list_devices(Duration::from_secs(15))?;
    let Some(device) = devices.first() else {
        bail!("SANE hat keinen Scanner gefunden.");
    };
    println!("Verwende Scanner: {}", device.label());
    Ok(device.name.clone())
}

fn acquire(options: &ScannerOptions) -> Result<(TempDir, PathBuf)> {
    let device = scanner_name(options)?;
    let temporary = TempDir::with_prefix("photoscanner-")?;
    let scan = temporary.path().join("scan.png");
    println!("Scanne mit {} dpi ...", options.dpi);
    scan_to_file(&scan, Some(&device), options.dpi, Duration::from_secs(600))?;
    Ok((temporary, scan))
}

pub fn run(command: Command) -> Result<()> {
    match command {
        Command::Gui => unreachable!("GUI wird in main behandelt"),
        Command::Devices => {
            let devices = list_devices(Duration::from_secs(15))?;
            if devices.is_empty() {
                bail!("Kein Scanner erkannt.");
            }
            for device in devices {
                println!(
                    "{}\t{}\t{}\t{}",
                    device.name, device.vendor, device.model, device.kind
                );
            }
        }
        Command::Split {
            source,
            split,
            export,
        } => {
            let result = split_scan(
                &source,
                &export.output,
                &config(&export, Some(&split), None),
                export.prefix.as_deref(),
                !split.no_preview,
            )?;
            print_split_result(&result);
        }
        Command::Scan {
            scanner,
            split,
            export,
        } => {
            let (_temporary, source) = acquire(&scanner)?;
            let result = split_scan(
                &source,
                &export.output,
                &config(&export, Some(&split), Some(scanner.dpi)),
                export.prefix.as_deref(),
                !split.no_preview,
            )?;
            print_split_result(&result);
        }
        Command::ScanFull { scanner, export } => {
            let (_temporary, source) = acquire(&scanner)?;
            let path = save_full_scan(
                &source,
                &export.output,
                &config(&export, None, Some(scanner.dpi)),
                export.prefix.as_deref(),
            )
            .context("Vollständiger Scan konnte nicht gespeichert werden")?;
            println!("Vollständiger Scan gespeichert:\n{}", path.display());
        }
    }
    Ok(())
}

fn print_split_result(result: &photoscanner::splitter::SplitResult) {
    println!("{} Foto(s) gespeichert:", result.files.len());
    for path in &result.files {
        println!("{}", path.display());
    }
    if let Some(preview) = &result.preview {
        println!("Vorschau: {}", preview.display());
    }
    println!("Erkennungsschwellwert: {:.1}", result.threshold_used);
}
