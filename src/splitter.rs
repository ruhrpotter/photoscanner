use std::cmp::Ordering;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use chrono::{DateTime, Local, NaiveDate, TimeZone, Timelike};
use opencv::core::{
    self, CV_8UC1, Mat, Point, Point2f, RotatedRect, Scalar, Size, Size2f, Vec3b, Vector,
};
use opencv::geometry;
use opencv::imgcodecs;
use opencv::imgproc;
use opencv::prelude::*;
use tempfile::{Builder as TempFileBuilder, NamedTempFile, TempPath};
use thiserror::Error;

const MIN_DETECTION_SIZE: i32 = 256;
const MAX_DETECTION_SIZE: i32 = 4096;
const MAX_INPUT_DIMENSION: u32 = 30_000;
const MAX_INPUT_PIXELS: u64 = 180_000_000;
const MAX_IMAGE_HEADER_BYTES: u64 = 16 * 1024 * 1024;
const MAX_PREFIX_BYTES: usize = 128;
const MAX_PREVIEW_SIZE: i32 = 2000;

#[derive(Debug, Error)]
/// Fehler bei der Validierung, Bildanalyse oder Veröffentlichung von Scans.
pub enum SplitError {
    /// Eine Konfiguration oder Eingabeeigenschaft ist ungültig.
    #[error("{0}")]
    InvalidConfig(String),
    /// Die angegebene Quelldatei existiert nicht.
    #[error("Die Eingabedatei existiert nicht: {0}")]
    MissingSource(PathBuf),
    /// OpenCV konnte ein Bild nicht verarbeiten.
    #[error("Bild konnte nicht verarbeitet werden: {0}")]
    OpenCv(#[from] opencv::Error),
    /// Eine Dateisystemoperation ist fehlgeschlagen.
    #[error("Dateioperation fehlgeschlagen: {0}")]
    Io(#[from] std::io::Error),
    /// Metadaten konnten nicht gelesen oder geschrieben werden.
    #[error("Bildmetadaten konnten nicht verarbeitet werden: {0}")]
    Metadata(String),
    /// Im Scan wurde kein einzelnes Foto erkannt.
    #[error(
        "Keine einzelnen Fotos erkannt. Lege zwischen den Fotos Abstand frei oder versuche einen kleineren Schwellwert."
    )]
    NothingDetected,
    /// Das Aufnahmedatum lässt sich in der lokalen Zeitzone nicht darstellen.
    #[error("Das ausgewählte Aufnahmedatum ist in der lokalen Zeitzone ungültig.")]
    InvalidDate,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
/// Unterstützte Ausgabeformate für Scans und getrennte Fotos.
pub enum OutputFormat {
    /// JPEG mit einstellbarer Qualität.
    #[default]
    Jpeg,
    /// Verlustfreies PNG.
    Png,
    /// Verlustfrei komprimiertes TIFF.
    Tiff,
}

impl OutputFormat {
    /// Liefert die kanonische Dateiendung ohne Punkt.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Png => "png",
            Self::Tiff => "tif",
        }
    }
}

impl std::fmt::Display for OutputFormat {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Jpeg => "JPG",
            Self::Png => "PNG",
            Self::Tiff => "TIFF",
        })
    }
}

impl FromStr for OutputFormat {
    type Err = SplitError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" => Ok(Self::Jpeg),
            "png" => Ok(Self::Png),
            "tif" | "tiff" => Ok(Self::Tiff),
            _ => Err(SplitError::InvalidConfig(
                "Unterstützte Formate sind JPG, PNG und TIFF.".to_string(),
            )),
        }
    }
}

#[derive(Clone, Debug)]
/// Parameter für Fotoerkennung und Export.
pub struct SplitConfig {
    /// Kleinste akzeptierte Fotofläche relativ zur Scanfläche in Prozent.
    pub min_area_percent: f64,
    /// Manueller Schwellwert oder `None` für automatische Bestimmung.
    pub threshold: Option<f64>,
    /// Zusätzlicher Rand um erkannte Fotos in Prozent.
    pub padding_percent: f64,
    /// Maximale Kantenlänge des verkleinerten Erkennungsbilds.
    pub max_detection_size: i32,
    /// Dateiformat der exportierten Bilder.
    pub output_format: OutputFormat,
    /// JPEG-Qualität von 1 bis 100.
    pub jpeg_quality: i32,
    /// Zu schreibende Auflösung oder `None` zur Übernahme aus der Quelle.
    pub dpi: Option<u32>,
    /// Aufnahmedatum oder `None` für das aktuelle lokale Datum.
    pub capture_date: Option<NaiveDate>,
}

impl Default for SplitConfig {
    fn default() -> Self {
        Self {
            min_area_percent: 2.0,
            threshold: None,
            padding_percent: 1.2,
            max_detection_size: 1800,
            output_format: OutputFormat::Jpeg,
            jpeg_quality: 95,
            dpi: None,
            capture_date: None,
        }
    }
}

impl SplitConfig {
    /// Prüft alle Werte gegen die unterstützten und ressourcensicheren Grenzen.
    pub fn validate(&self) -> Result<(), SplitError> {
        if !(0.1..=50.0).contains(&self.min_area_percent) {
            return Err(SplitError::InvalidConfig(
                "Die Mindestfläche muss zwischen 0,1 und 50 Prozent liegen.".to_string(),
            ));
        }
        if self
            .threshold
            .is_some_and(|value| !(1.0..=255.0).contains(&value))
        {
            return Err(SplitError::InvalidConfig(
                "Der Schwellwert muss zwischen 1 und 255 liegen.".to_string(),
            ));
        }
        if !(0.0..=15.0).contains(&self.padding_percent) {
            return Err(SplitError::InvalidConfig(
                "Der Rand muss zwischen 0 und 15 Prozent liegen.".to_string(),
            ));
        }
        if !(MIN_DETECTION_SIZE..=MAX_DETECTION_SIZE).contains(&self.max_detection_size) {
            return Err(SplitError::InvalidConfig(format!(
                "Die maximale Erkennungsgröße muss zwischen {MIN_DETECTION_SIZE} und {MAX_DETECTION_SIZE} Pixeln liegen."
            )));
        }
        if !(1..=100).contains(&self.jpeg_quality) {
            return Err(SplitError::InvalidConfig(
                "Die JPEG-Qualität muss zwischen 1 und 100 liegen.".to_string(),
            ));
        }
        Ok(())
    }
}

fn validate_prefix(prefix: &str) -> Result<(), SplitError> {
    let mut components = Path::new(prefix).components();
    let is_single_normal_component = matches!(components.next(), Some(Component::Normal(value)) if value == prefix)
        && components.next().is_none();
    if !is_single_normal_component
        || prefix.len() > MAX_PREFIX_BYTES
        || prefix.chars().any(char::is_control)
        || prefix.contains('\\')
    {
        return Err(SplitError::InvalidConfig(format!(
            "Der Dateipräfix muss aus genau einem sicheren Dateinamen mit höchstens {MAX_PREFIX_BYTES} Bytes bestehen."
        )));
    }
    Ok(())
}

fn validate_image_dimensions(width: u32, height: u32) -> Result<(), SplitError> {
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| {
            SplitError::InvalidConfig("Die Bildabmessungen sind ungültig.".to_string())
        })?;
    if width < 20 || height < 20 {
        return Err(SplitError::InvalidConfig(
            "Das Scanbild ist zu klein oder hat ein ungültiges Format.".to_string(),
        ));
    }
    if width > MAX_INPUT_DIMENSION || height > MAX_INPUT_DIMENSION || pixels > MAX_INPUT_PIXELS {
        return Err(SplitError::InvalidConfig(format!(
            "Das Bild ist mit {width}×{height} Pixeln zu groß. Erlaubt sind höchstens {MAX_INPUT_PIXELS} Pixel und {MAX_INPUT_DIMENSION} Pixel je Kante."
        )));
    }
    Ok(())
}

fn read_u16(bytes: [u8; 2], little_endian: bool) -> u16 {
    if little_endian {
        u16::from_le_bytes(bytes)
    } else {
        u16::from_be_bytes(bytes)
    }
}

fn read_u32(bytes: [u8; 4], little_endian: bool) -> u32 {
    if little_endian {
        u32::from_le_bytes(bytes)
    } else {
        u32::from_be_bytes(bytes)
    }
}

fn jpeg_dimensions(file: &mut File) -> Result<Option<(u32, u32)>, std::io::Error> {
    let mut file = BufReader::new(file);
    file.seek(SeekFrom::Start(2))?;
    let mut position = 2u64;
    loop {
        if position >= MAX_IMAGE_HEADER_BYTES {
            return Ok(None);
        }
        let mut byte = [0u8; 1];
        file.read_exact(&mut byte)?;
        position += 1;
        while byte[0] != 0xff {
            if position >= MAX_IMAGE_HEADER_BYTES {
                return Ok(None);
            }
            file.read_exact(&mut byte)?;
            position += 1;
        }
        while byte[0] == 0xff {
            if position >= MAX_IMAGE_HEADER_BYTES {
                return Ok(None);
            }
            file.read_exact(&mut byte)?;
            position += 1;
        }
        let marker = byte[0];
        if marker == 0xd9 || marker == 0xda {
            return Ok(None);
        }
        if marker == 0xd8 || marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        let mut length = [0u8; 2];
        file.read_exact(&mut length)?;
        position += 2;
        let length = u16::from_be_bytes(length);
        if length < 2 {
            return Ok(None);
        }
        if matches!(
            marker,
            0xc0 | 0xc1
                | 0xc2
                | 0xc3
                | 0xc5
                | 0xc6
                | 0xc7
                | 0xc9
                | 0xca
                | 0xcb
                | 0xcd
                | 0xce
                | 0xcf
        ) {
            if length < 7 {
                return Ok(None);
            }
            let mut dimensions = [0u8; 5];
            file.read_exact(&mut dimensions)?;
            let height = u16::from_be_bytes([dimensions[1], dimensions[2]]) as u32;
            let width = u16::from_be_bytes([dimensions[3], dimensions[4]]) as u32;
            return Ok(Some((width, height)));
        }
        let next_position = position
            .checked_add(u64::from(length - 2))
            .filter(|position| *position <= MAX_IMAGE_HEADER_BYTES);
        let Some(next_position) = next_position else {
            return Ok(None);
        };
        file.seek(SeekFrom::Start(next_position))?;
        position = next_position;
    }
}

fn tiff_dimensions(
    file: &mut File,
    header: &[u8; 8],
) -> Result<Option<(u32, u32)>, std::io::Error> {
    let little_endian = &header[..2] == b"II";
    if (!little_endian && &header[..2] != b"MM")
        || read_u16([header[2], header[3]], little_endian) != 42
    {
        return Ok(None);
    }
    let ifd_offset = read_u32(header[4..8].try_into().unwrap(), little_endian);
    if u64::from(ifd_offset) + 2 > MAX_IMAGE_HEADER_BYTES {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(u64::from(ifd_offset)))?;
    let mut entry_count = [0u8; 2];
    file.read_exact(&mut entry_count)?;
    let entry_count = read_u16(entry_count, little_endian);
    if u64::from(ifd_offset) + 2 + u64::from(entry_count) * 12 > MAX_IMAGE_HEADER_BYTES {
        return Ok(None);
    }
    let mut width = None;
    let mut height = None;
    for _ in 0..entry_count {
        let mut entry = [0u8; 12];
        file.read_exact(&mut entry)?;
        let tag = read_u16(entry[..2].try_into().unwrap(), little_endian);
        if tag != 256 && tag != 257 {
            continue;
        }
        let value_type = read_u16(entry[2..4].try_into().unwrap(), little_endian);
        let count = read_u32(entry[4..8].try_into().unwrap(), little_endian);
        if count != 1 {
            return Ok(None);
        }
        let value = match value_type {
            3 => u32::from(read_u16(entry[8..10].try_into().unwrap(), little_endian)),
            4 => read_u32(entry[8..12].try_into().unwrap(), little_endian),
            _ => return Ok(None),
        };
        if tag == 256 {
            width = Some(value);
        } else {
            height = Some(value);
        }
        if width.is_some() && height.is_some() {
            break;
        }
    }
    Ok(width.zip(height))
}

fn image_dimensions(path: &Path) -> Result<(u32, u32), SplitError> {
    let mut file = File::open(path)?;
    let mut header = [0u8; 24];
    file.read_exact(&mut header).map_err(|error| {
        SplitError::InvalidConfig(format!("Bildkopf konnte nicht gelesen werden: {error}"))
    })?;
    let dimensions = if header.starts_with(b"\x89PNG\r\n\x1a\n") {
        (&header[12..16] == b"IHDR").then(|| {
            (
                u32::from_be_bytes(header[16..20].try_into().unwrap()),
                u32::from_be_bytes(header[20..24].try_into().unwrap()),
            )
        })
    } else if header.starts_with(b"\xff\xd8") {
        jpeg_dimensions(&mut file)?
    } else if header.starts_with(b"II") || header.starts_with(b"MM") {
        tiff_dimensions(&mut file, header[..8].try_into().unwrap())?
    } else {
        None
    };
    dimensions.ok_or_else(|| {
        SplitError::InvalidConfig(
            "Bildabmessungen konnten vor dem Laden nicht sicher geprüft werden. Unterstützt werden PNG, JPEG und TIFF."
                .to_string(),
        )
    })
}

#[derive(Debug)]
/// Ein erkanntes und perspektivisch korrigiertes Foto.
pub struct DetectedPhoto {
    /// Aus der Scanfläche ausgeschnittenes und begradigtes Bild.
    pub image: Mat,
    /// Mittelpunkt des Fotos im Koordinatensystem des Quellbilds.
    pub center: Point2f,
    /// Vier Eckpunkte des Fotos im Quellbild.
    pub source_box: [Point2f; 4],
    /// Flächenanteil des Fotos an der gesamten Scanfläche in Prozent.
    pub area_percent: f64,
}

#[derive(Clone, Debug)]
/// Geometrie eines auf der Scanfläche erkannten Fotos.
pub struct DetectedRegion {
    /// Mittelpunkt im Koordinatensystem des Quellbilds.
    pub center: Point2f,
    /// Vier geordnete Eckpunkte im Quellbild.
    pub source_box: [Point2f; 4],
    /// Flächenanteil an der gesamten Scanfläche in Prozent.
    pub area_percent: f64,
}

#[derive(Debug)]
/// Geladenes Quellbild mit Erkennungsgeometrie und Metadaten.
pub struct AnalyzedScan {
    /// Orientierungsbereinigtes Quellbild.
    pub image: Mat,
    /// Zeilenweise sortierte erkannte Fotoregionen.
    pub regions: Vec<DetectedRegion>,
    /// Tatsächlich verwendeter Erkennungsschwellwert.
    pub threshold: f64,
    /// Aus Container oder Metadaten gelesene Auflösung.
    pub embedded_dpi: Option<u32>,
}

#[derive(Clone, Debug)]
/// Ergebnis einer vollständig veröffentlichten Scanaufteilung.
pub struct SplitResult {
    /// Kollisionsfrei angelegte Bilddateien.
    pub files: Vec<PathBuf>,
    /// Optionale, begrenzte Kontrollvorschau mit Markierungen.
    pub preview: Option<PathBuf>,
    /// Tatsächlich verwendeter Erkennungsschwellwert.
    pub threshold_used: f64,
}

fn metadata_error(error: impl std::fmt::Display) -> SplitError {
    SplitError::Metadata(error.to_string())
}

fn image_dpi(path: &Path) -> Result<Option<u32>, SplitError> {
    Ok(crate::metadata::image_dpi(path)
        .map_err(metadata_error)?
        .or_else(|| container_dpi(path)))
}

fn container_dpi(path: &Path) -> Option<u32> {
    let mut prefix = Vec::new();
    File::open(path)
        .ok()?
        .take(1024 * 1024)
        .read_to_end(&mut prefix)
        .ok()?;
    if prefix.starts_with(b"\x89PNG\r\n\x1a\n") {
        let mut position = 8usize;
        while position.checked_add(12)? <= prefix.len() {
            let length =
                u32::from_be_bytes(prefix[position..position + 4].try_into().ok()?) as usize;
            let kind = &prefix[position + 4..position + 8];
            let data_start = position + 8;
            let data_end = data_start.checked_add(length)?;
            if data_end.checked_add(4)? > prefix.len() {
                break;
            }
            if kind == b"pHYs" && length >= 9 && prefix[data_start + 8] == 1 {
                let pixels_per_meter =
                    u32::from_be_bytes(prefix[data_start..data_start + 4].try_into().ok()?);
                return Some((pixels_per_meter as f64 * 0.0254).round() as u32);
            }
            if kind == b"IDAT" {
                break;
            }
            position = data_end + 4;
        }
    } else if prefix.starts_with(b"\xff\xd8")
        && let Some(position) = prefix.windows(5).position(|window| window == b"JFIF\0")
        && position + 12 <= prefix.len()
        && prefix[position + 7] == 1
    {
        return Some(u16::from_be_bytes([prefix[position + 8], prefix[position + 9]]) as u32);
    }
    None
}

fn read_image(path: &Path) -> Result<(Mat, Option<u32>), SplitError> {
    if !path.is_file() {
        return Err(SplitError::MissingSource(path.to_path_buf()));
    }
    let (width, height) = image_dimensions(path)?;
    validate_image_dimensions(width, height)?;
    let mut image = imgcodecs::imread(
        path,
        imgcodecs::IMREAD_COLOR | imgcodecs::IMREAD_IGNORE_ORIENTATION,
    )?;
    if image.empty()
        || image.rows() < 20
        || image.cols() < 20
        || image.cols() as u32 != width
        || image.rows() as u32 != height
    {
        return Err(SplitError::InvalidConfig(
            "Das Scanbild hat ein ungültiges Format oder widersprüchliche Abmessungen.".to_string(),
        ));
    }
    let orientation = crate::metadata::image_orientation(path).map_err(metadata_error)?;
    let dpi = image_dpi(path)?;
    if let Some(orientation) = orientation {
        let mut transformed = Mat::default();
        match orientation {
            2 => core::flip(&image, &mut transformed, 1)?,
            3 => core::rotate(&image, &mut transformed, core::ROTATE_180)?,
            4 => core::flip(&image, &mut transformed, 0)?,
            5 => core::transpose(&image, &mut transformed)?,
            6 => core::rotate(&image, &mut transformed, core::ROTATE_90_CLOCKWISE)?,
            7 => {
                let mut transposed = Mat::default();
                core::transpose(&image, &mut transposed)?;
                core::flip(&transposed, &mut transformed, -1)?;
            }
            8 => core::rotate(&image, &mut transformed, core::ROTATE_90_COUNTERCLOCKWISE)?,
            _ => return Ok((image, dpi)),
        }
        image = transformed;
    }
    Ok((image, dpi))
}

fn scaled_for_detection(image: &Mat, maximum: i32) -> Result<(Mat, f32), SplitError> {
    let largest = image.rows().max(image.cols());
    let scale = (maximum as f32 / largest as f32).min(1.0);
    if (scale - 1.0).abs() < f32::EPSILON {
        return Ok((image.try_clone()?, scale));
    }
    let mut resized = Mat::default();
    imgproc::resize(
        image,
        &mut resized,
        Size::new(
            ((image.cols() as f32 * scale).round() as i32).max(1),
            ((image.rows() as f32 * scale).round() as i32).max(1),
        ),
        0.0,
        0.0,
        imgproc::INTER_AREA,
    )?;
    Ok((resized, scale))
}

fn percentile(values: &mut [f32], percent: f32) -> f32 {
    values.sort_by(|left, right| left.partial_cmp(right).unwrap_or(Ordering::Equal));
    let index = (((values.len().saturating_sub(1)) as f32) * percent)
        .round()
        .clamp(0.0, values.len().saturating_sub(1) as f32) as usize;
    values.get(index).copied().unwrap_or(0.0)
}

fn foreground_mask(image: &Mat, override_threshold: Option<f64>) -> Result<(Mat, f64), SplitError> {
    let mut blurred = Mat::default();
    imgproc::gaussian_blur_def(image, &mut blurred, Size::new(5, 5), 0.0)?;
    let mut lab = Mat::default();
    imgproc::cvt_color_def(&blurred, &mut lab, imgproc::COLOR_BGR2Lab)?;

    let rows = lab.rows();
    let cols = lab.cols();
    let strip = ((rows.min(cols) as f32 * 0.015).round() as i32).max(2);
    let mut samples = Vec::with_capacity(((rows + cols) * strip * 2) as usize);
    for y in 0..rows {
        for x in 0..cols {
            if y < strip || y >= rows - strip || x < strip || x >= cols - strip {
                samples.push(*lab.at_2d::<Vec3b>(y, x)?);
            }
        }
    }
    let mut channel_l: Vec<u8> = samples.iter().map(|value| value[0]).collect();
    let mut channel_a: Vec<u8> = samples.iter().map(|value| value[1]).collect();
    let mut channel_b: Vec<u8> = samples.iter().map(|value| value[2]).collect();
    channel_l.sort_unstable();
    channel_a.sort_unstable();
    channel_b.sort_unstable();
    let middle = samples.len() / 2;
    let background = [
        channel_l[middle] as f32,
        channel_a[middle] as f32,
        channel_b[middle] as f32,
    ];
    let distance_from = |pixel: Vec3b| {
        let dl = pixel[0] as f32 - background[0];
        let da = pixel[1] as f32 - background[1];
        let db = pixel[2] as f32 - background[2];
        (dl * dl + da * da + db * db).sqrt()
    };
    let mut border_distances: Vec<f32> = samples.iter().copied().map(distance_from).collect();
    let threshold = override_threshold.unwrap_or_else(|| {
        let noise = percentile(&mut border_distances, 0.95) as f64;
        (noise * 2.5).max(12.0).clamp(12.0, 48.0)
    });

    let mut mask = Mat::zeros(rows, cols, CV_8UC1)?.to_mat()?;
    for y in 0..rows {
        for x in 0..cols {
            if distance_from(*lab.at_2d::<Vec3b>(y, x)?) >= threshold as f32 {
                *mask.at_2d_mut::<u8>(y, x)? = 255;
            }
        }
    }
    let short_side = rows.min(cols);
    let odd_size = |ratio: f32| {
        let value = ((short_side as f32 * ratio).round() as i32).max(3);
        if value % 2 == 0 { value + 1 } else { value }
    };
    let close_kernel = imgproc::get_structuring_element_def(
        imgproc::MORPH_ELLIPSE,
        Size::new(odd_size(0.004), odd_size(0.004)),
    )?;
    let open_kernel = imgproc::get_structuring_element_def(
        imgproc::MORPH_ELLIPSE,
        Size::new(odd_size(0.0025), odd_size(0.0025)),
    )?;
    let mut closed = Mat::default();
    imgproc::morphology_ex_def(&mask, &mut closed, imgproc::MORPH_CLOSE, &close_kernel)?;
    let mut opened = Mat::default();
    imgproc::morphology_ex_def(&closed, &mut opened, imgproc::MORPH_OPEN, &open_kernel)?;

    let border = ((short_side as f32 * 0.01).round() as i32).max(3);
    for y in 0..rows {
        for x in 0..cols {
            if y < border || y >= rows - border || x < border || x >= cols - border {
                *opened.at_2d_mut::<u8>(y, x)? = 0;
            }
        }
    }
    Ok((opened, threshold))
}

fn order_box(points: [Point2f; 4]) -> [Point2f; 4] {
    let min_sum = *points
        .iter()
        .min_by(|a, b| {
            (a.x + a.y)
                .partial_cmp(&(b.x + b.y))
                .unwrap_or(Ordering::Equal)
        })
        .unwrap();
    let max_sum = *points
        .iter()
        .max_by(|a, b| {
            (a.x + a.y)
                .partial_cmp(&(b.x + b.y))
                .unwrap_or(Ordering::Equal)
        })
        .unwrap();
    let min_diff = *points
        .iter()
        .min_by(|a, b| {
            (a.y - a.x)
                .partial_cmp(&(b.y - b.x))
                .unwrap_or(Ordering::Equal)
        })
        .unwrap();
    let max_diff = *points
        .iter()
        .max_by(|a, b| {
            (a.y - a.x)
                .partial_cmp(&(b.y - b.x))
                .unwrap_or(Ordering::Equal)
        })
        .unwrap();
    [min_sum, min_diff, max_sum, max_diff]
}

fn point_distance(left: Point2f, right: Point2f) -> f32 {
    ((left.x - right.x).powi(2) + (left.y - right.y).powi(2)).sqrt()
}

fn warp_photo(image: &Mat, source_box: [Point2f; 4]) -> Result<Mat, SplitError> {
    let ordered = order_box(source_box);
    let width = point_distance(ordered[1], ordered[0])
        .max(point_distance(ordered[2], ordered[3]))
        .round()
        .max(1.0) as i32;
    let height = point_distance(ordered[3], ordered[0])
        .max(point_distance(ordered[2], ordered[1]))
        .round()
        .max(1.0) as i32;
    let target = [
        Point2f::new(0.0, 0.0),
        Point2f::new((width - 1) as f32, 0.0),
        Point2f::new((width - 1) as f32, (height - 1) as f32),
        Point2f::new(0.0, (height - 1) as f32),
    ];
    let transform = geometry::get_perspective_transform_slice_def(ordered, target)?;
    let mut warped = Mat::default();
    imgproc::warp_perspective_def(image, &mut warped, &transform, Size::new(width, height))?;
    Ok(warped)
}

fn detect_photo_regions(
    image: &Mat,
    config: &SplitConfig,
) -> Result<(Vec<DetectedRegion>, f64), SplitError> {
    config.validate()?;
    let (detection_image, scale) = scaled_for_detection(image, config.max_detection_size)?;
    let (mask, threshold) = foreground_mask(&detection_image, config.threshold)?;
    drop(detection_image);
    let mut contours: Vector<Vector<Point>> = Vector::new();
    imgproc::find_contours_def(
        &mask,
        &mut contours,
        imgproc::RETR_EXTERNAL,
        imgproc::CHAIN_APPROX_SIMPLE,
    )?;
    let scan_area = (mask.rows() * mask.cols()) as f64;
    let minimum_area = scan_area * config.min_area_percent / 100.0;
    let mut detected = Vec::new();
    for contour in contours {
        let area = geometry::contour_area(&contour, false)?;
        if area < minimum_area || area > scan_area * 0.97 {
            continue;
        }
        let rectangle = geometry::min_area_rect(&contour)?;
        if rectangle.size.width.min(rectangle.size.height)
            < (mask.rows().min(mask.cols()) as f32 * 0.04).max(12.0)
        {
            continue;
        }
        let factor = 1.0 + 2.0 * config.padding_percent as f32 / 100.0;
        let expanded = RotatedRect::new(
            rectangle.center,
            Size2f::new(
                rectangle.size.width * factor,
                rectangle.size.height * factor,
            ),
            rectangle.angle,
        )?;
        let mut small_box = [Point2f::default(); 4];
        expanded.points(&mut small_box)?;
        let mut source_box = small_box.map(|point| {
            Point2f::new(
                (point.x / scale).clamp(0.0, (image.cols() - 1) as f32),
                (point.y / scale).clamp(0.0, (image.rows() - 1) as f32),
            )
        });
        // Stable order makes previews and row sorting deterministic across OpenCV versions.
        source_box = order_box(source_box);
        detected.push(DetectedRegion {
            center: Point2f::new(rectangle.center.x / scale, rectangle.center.y / scale),
            source_box,
            area_percent: area / scan_area * 100.0,
        });
    }
    if !detected.is_empty() {
        let mut heights: Vec<f32> = detected
            .iter()
            .map(|photo| {
                let min = photo
                    .source_box
                    .iter()
                    .map(|point| point.y)
                    .fold(f32::INFINITY, f32::min);
                let max = photo
                    .source_box
                    .iter()
                    .map(|point| point.y)
                    .fold(f32::NEG_INFINITY, f32::max);
                max - min
            })
            .collect();
        let row_height = percentile(&mut heights, 0.5).max(1.0) * 0.5;
        detected.sort_by(|left, right| {
            let left_row = (left.center.y / row_height).round() as i32;
            let right_row = (right.center.y / row_height).round() as i32;
            left_row.cmp(&right_row).then_with(|| {
                left.center
                    .x
                    .partial_cmp(&right.center.x)
                    .unwrap_or(Ordering::Equal)
            })
        });
    }
    Ok((detected, threshold))
}

/// Erkennt Fotos in einem bereits geladenen Bild und begradigt sie.
///
/// Die Reihenfolge verläuft zeilenweise von links nach rechts.
pub fn detect_photos(
    image: &Mat,
    config: &SplitConfig,
) -> Result<(Vec<DetectedPhoto>, f64), SplitError> {
    let (regions, threshold) = detect_photo_regions(image, config)?;
    let mut photos = Vec::with_capacity(regions.len());
    for region in regions {
        let photo = warp_photo(image, region.source_box)?;
        if photo.rows().min(photo.cols()) < 10 {
            continue;
        }
        photos.push(DetectedPhoto {
            image: photo,
            center: region.center,
            source_box: region.source_box,
            area_percent: region.area_percent,
        });
    }
    Ok((photos, threshold))
}

/// Lädt und analysiert eine Scandatei, ohne Ausgabedateien anzulegen.
pub fn analyze_scan(source: &Path, config: &SplitConfig) -> Result<AnalyzedScan, SplitError> {
    config.validate()?;
    let (image, embedded_dpi) = read_image(source)?;
    let (regions, threshold) = detect_photo_regions(&image, config)?;
    if regions.is_empty() {
        return Err(SplitError::NothingDetected);
    }
    Ok(AnalyzedScan {
        image,
        regions,
        threshold,
        embedded_dpi,
    })
}

/// Schneidet eine erkannte Region perspektivisch korrigiert aus.
pub fn warp_detected_photo(
    analyzed: &AnalyzedScan,
    region: &DetectedRegion,
) -> Result<Mat, SplitError> {
    warp_photo(&analyzed.image, region.source_box)
}

struct StagedOutput {
    temporary: TempPath,
    stem: String,
    extension: &'static str,
}

fn output_path(directory: &Path, output: &StagedOutput, attempt: usize) -> PathBuf {
    let stem = if attempt == 1 {
        output.stem.clone()
    } else {
        format!("{}_{attempt}", output.stem)
    };
    directory.join(format!("{stem}.{}", output.extension))
}

fn rollback_files(paths: &[PathBuf]) -> Result<(), std::io::Error> {
    let mut first_error = None;
    for path in paths.iter().rev() {
        if let Err(error) = fs::remove_file(path)
            && error.kind() != std::io::ErrorKind::NotFound
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn commit_staged(staged: &[StagedOutput], destinations: &[PathBuf]) -> Result<(), std::io::Error> {
    debug_assert_eq!(staged.len(), destinations.len());
    let mut published = Vec::with_capacity(staged.len());
    for (output, destination) in staged.iter().zip(destinations) {
        if let Err(publish_error) = fs::hard_link(&output.temporary, destination) {
            if let Err(rollback_error) = rollback_files(&published) {
                return Err(std::io::Error::new(
                    rollback_error.kind(),
                    format!(
                        "Veröffentlichen schlug fehl ({publish_error}); Zurückrollen schlug ebenfalls fehl ({rollback_error})"
                    ),
                ));
            }
            return Err(publish_error);
        }
        published.push(destination.clone());
    }
    Ok(())
}

fn publish_staged_group(
    directory: &Path,
    staged: Vec<StagedOutput>,
) -> Result<Vec<PathBuf>, SplitError> {
    let mut attempt = 1usize;
    loop {
        let destinations: Vec<_> = staged
            .iter()
            .map(|output| output_path(directory, output, attempt))
            .collect();
        match commit_staged(&staged, &destinations) {
            Ok(()) => {
                if let Err(error) = File::open(directory).and_then(|directory| directory.sync_all())
                {
                    rollback_files(&destinations)?;
                    return Err(SplitError::Io(error));
                }
                return Ok(destinations);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                attempt = attempt.checked_add(1).ok_or_else(|| {
                    SplitError::InvalidConfig(
                        "Es konnte kein freier Dateiname ermittelt werden.".to_string(),
                    )
                })?;
            }
            Err(error) => return Err(SplitError::Io(error)),
        }
    }
}

fn capture_datetime(date: Option<NaiveDate>) -> Result<DateTime<Local>, SplitError> {
    let now = Local::now();
    let Some(date) = date else {
        return Ok(now);
    };
    let local = date.and_time(now.time().with_nanosecond(0).unwrap_or(now.time()));
    Local
        .from_local_datetime(&local)
        .earliest()
        .ok_or(SplitError::InvalidDate)
}

fn write_container_dpi(path: &Path, format: OutputFormat, dpi: u32) -> Result<(), SplitError> {
    match format {
        OutputFormat::Jpeg => write_jpeg_density(path, dpi),
        OutputFormat::Png => write_png_density(path, dpi),
        OutputFormat::Tiff => Ok(()),
    }
}

fn write_jpeg_density(path: &Path, dpi: u32) -> Result<(), SplitError> {
    let mut prefix = Vec::new();
    File::open(path)?
        .take(1024 * 1024)
        .read_to_end(&mut prefix)?;
    let Some(position) = prefix.windows(5).position(|window| window == b"JFIF\0") else {
        return Ok(());
    };
    if position + 12 > prefix.len() {
        return Ok(());
    }
    let density = u16::try_from(dpi).unwrap_or(u16::MAX).to_be_bytes();
    let mut file = OpenOptions::new().write(true).open(path)?;
    file.seek(SeekFrom::Start((position + 7) as u64))?;
    file.write_all(&[1, density[0], density[1], density[0], density[1]])?;
    Ok(())
}

fn write_png_density(path: &Path, dpi: u32) -> Result<(), SplitError> {
    let mut source = File::open(path)?;
    let mut header = [0u8; 33];
    source.read_exact(&mut header)?;
    if !header.starts_with(b"\x89PNG\r\n\x1a\n") || &header[12..16] != b"IHDR" {
        return Ok(());
    }

    let pixels_per_meter = (dpi as f64 / 0.0254).round() as u32;
    let mut chunk_data = Vec::with_capacity(13);
    chunk_data.extend_from_slice(b"pHYs");
    chunk_data.extend_from_slice(&pixels_per_meter.to_be_bytes());
    chunk_data.extend_from_slice(&pixels_per_meter.to_be_bytes());
    chunk_data.push(1);
    let crc = crc32fast::hash(&chunk_data);

    let temporary = path.with_extension(format!(
        "{}.dpi-part",
        path.extension()
            .and_then(|value| value.to_str())
            .unwrap_or("png")
    ));
    let result = (|| -> Result<(), std::io::Error> {
        let mut output = File::create(&temporary)?;
        output.write_all(&header)?;
        output.write_all(&9u32.to_be_bytes())?;
        output.write_all(&chunk_data)?;
        output.write_all(&crc.to_be_bytes())?;
        std::io::copy(&mut source, &mut output)?;
        output.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(SplitError::Io)
}

fn save_image(
    image: &Mat,
    path: &Path,
    format: OutputFormat,
    quality: i32,
    dpi: Option<u32>,
    captured_at: DateTime<Local>,
) -> Result<(), SplitError> {
    // TIFF requires physical resolution tags even when the source did not
    // provide any. 72 dpi is the conventional neutral fallback; scanned and
    // imported files keep their real value instead.
    let metadata_dpi = dpi.or((format == OutputFormat::Tiff).then_some(72));
    let mut params = Vector::<i32>::new();
    match format {
        OutputFormat::Jpeg => {
            params.push(imgcodecs::IMWRITE_JPEG_QUALITY);
            params.push(quality);
            params.push(imgcodecs::IMWRITE_JPEG_OPTIMIZE);
            params.push(1);
        }
        OutputFormat::Png => {
            params.push(imgcodecs::IMWRITE_PNG_COMPRESSION);
            params.push(4);
        }
        OutputFormat::Tiff => {
            params.push(imgcodecs::IMWRITE_TIFF_COMPRESSION);
            params.push(imgcodecs::IMWRITE_TIFF_COMPRESSION_LZW);
            if let Some(dpi) = metadata_dpi {
                params.push(imgcodecs::IMWRITE_TIFF_RESUNIT);
                params.push(2);
                params.push(imgcodecs::IMWRITE_TIFF_XDPI);
                params.push(dpi as i32);
                params.push(imgcodecs::IMWRITE_TIFF_YDPI);
                params.push(dpi as i32);
            }
        }
    }
    if !imgcodecs::imwrite(path, image, &params)? {
        return Err(SplitError::InvalidConfig(format!(
            "Bild konnte nicht gespeichert werden: {}",
            path.display()
        )));
    }
    if let Some(dpi) = metadata_dpi
        && let Err(error) = write_container_dpi(path, format, dpi)
    {
        let _ = fs::remove_file(path);
        return Err(error);
    }
    if let Err(error) = crate::metadata::write_metadata(path, captured_at, metadata_dpi) {
        let _ = fs::remove_file(path);
        return Err(metadata_error(error));
    }
    Ok(())
}

fn output_tempfile(directory: &Path, suffix: &str) -> Result<NamedTempFile, std::io::Error> {
    let mut builder = TempFileBuilder::new();
    builder.prefix(".photoscanner-").suffix(suffix);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o666));
    }
    builder.tempfile_in(directory)
}

fn stage_image(
    image: &Mat,
    directory: &Path,
    format: OutputFormat,
    quality: i32,
    dpi: Option<u32>,
    captured_at: DateTime<Local>,
) -> Result<TempPath, SplitError> {
    let suffix = format!(".{}", format.extension());
    let temporary = output_tempfile(directory, &suffix)?;
    save_image(image, temporary.path(), format, quality, dpi, captured_at)?;
    File::open(temporary.path())?.sync_all()?;
    Ok(temporary.into_temp_path())
}

fn save_preview(
    image: &Mat,
    photos: &[DetectedRegion],
    path: &Path,
    captured_at: DateTime<Local>,
) -> Result<(), SplitError> {
    let (mut overlay, scale) = scaled_for_detection(image, MAX_PREVIEW_SIZE)?;
    let font_scale = (overlay.rows().min(overlay.cols()) as f64 / 1400.0).clamp(0.7, 4.0);
    let thickness = (font_scale * 2.0).round().max(2.0) as i32;
    for (index, photo) in photos.iter().enumerate() {
        let polygon: Vector<Point> = photo
            .source_box
            .iter()
            .map(|point| {
                Point::new(
                    (point.x * scale).round() as i32,
                    (point.y * scale).round() as i32,
                )
            })
            .collect();
        let mut polygons: Vector<Vector<Point>> = Vector::new();
        polygons.push(polygon.clone());
        imgproc::polylines(
            &mut overlay,
            &polygons,
            true,
            Scalar::new(30.0, 190.0, 40.0, 0.0),
            thickness,
            imgproc::LINE_AA,
            0,
        )?;
        let anchor = polygon
            .iter()
            .min_by_key(|point| point.x + point.y)
            .unwrap_or_default();
        imgproc::put_text(
            &mut overlay,
            &(index + 1).to_string(),
            Point::new(anchor.x + 8, anchor.y + 28),
            imgproc::FONT_HERSHEY_SIMPLEX,
            font_scale,
            Scalar::new(20.0, 40.0, 230.0, 0.0),
            thickness,
            imgproc::LINE_AA,
            false,
        )?;
    }
    save_image(&overlay, path, OutputFormat::Jpeg, 88, None, captured_at)
}

fn stage_preview(
    image: &Mat,
    photos: &[DetectedRegion],
    directory: &Path,
    captured_at: DateTime<Local>,
) -> Result<TempPath, SplitError> {
    let temporary = output_tempfile(directory, ".jpg")?;
    save_preview(image, photos, temporary.path(), captured_at)?;
    File::open(temporary.path())?.sync_all()?;
    Ok(temporary.into_temp_path())
}

/// Schreibt eine begrenzte Kontrollvorschau für ausgewählte Regionen.
pub fn save_detection_preview(
    analyzed: &AnalyzedScan,
    regions: &[DetectedRegion],
    path: &Path,
    capture_date: Option<NaiveDate>,
) -> Result<(), SplitError> {
    save_preview(
        &analyzed.image,
        regions,
        path,
        capture_datetime(capture_date)?,
    )
}

/// Speichert die gesamte Scanfläche als eine neue, kollisionsfreie Datei.
///
/// `prefix` muss genau eine sichere Dateinamenskomponente enthalten. Ohne
/// Präfix wird ein zeitbasierter Name erzeugt.
pub fn save_full_scan(
    source: &Path,
    output_directory: &Path,
    config: &SplitConfig,
    prefix: Option<&str>,
) -> Result<PathBuf, SplitError> {
    config.validate()?;
    if let Some(prefix) = prefix {
        validate_prefix(prefix)?;
    }
    let (image, embedded_dpi) = read_image(source)?;
    fs::create_dir_all(output_directory)?;
    let captured_at = capture_datetime(config.capture_date)?;
    let base = prefix
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("scan_{}", captured_at.format("%Y%m%d_%H%M%S")));
    let temporary = stage_image(
        &image,
        output_directory,
        config.output_format,
        config.jpeg_quality,
        config.dpi.or(embedded_dpi),
        captured_at,
    )?;
    let mut paths = publish_staged_group(
        output_directory,
        vec![StagedOutput {
            temporary,
            stem: base,
            extension: config.output_format.extension(),
        }],
    )?;
    Ok(paths.pop().expect("eine bereitgestellte Ausgabedatei"))
}

/// Erkennt alle Fotos und veröffentlicht sie gemeinsam ohne Überschreiben.
///
/// Bei einem Veröffentlichungsfehler wird die komplette Gruppe zurückgerollt.
/// Optional wird eine größenbegrenzte Kontrollvorschau erzeugt.
pub fn split_scan(
    source: &Path,
    output_directory: &Path,
    config: &SplitConfig,
    prefix: Option<&str>,
    create_preview: bool,
) -> Result<SplitResult, SplitError> {
    let analyzed = analyze_scan(source, config)?;
    let mut photos = Vec::with_capacity(analyzed.regions.len());
    let mut exported_regions = Vec::with_capacity(analyzed.regions.len());
    for region in &analyzed.regions {
        let photo = warp_detected_photo(&analyzed, region)?;
        if photo.rows().min(photo.cols()) < 10 {
            continue;
        }
        photos.push(photo);
        exported_regions.push(region.clone());
    }
    export_photos(
        &photos,
        &analyzed,
        output_directory,
        config,
        prefix,
        create_preview.then_some(exported_regions.as_slice()),
    )
}

/// Veröffentlicht vorbereitete Fotos gemeinsam und kollisionsfrei.
///
/// Wenn `preview_regions` gesetzt ist, muss es genau eine Region pro Foto
/// enthalten. Die Vorschau markiert und nummeriert nur diese Regionen.
pub fn export_photos(
    photos: &[Mat],
    analyzed: &AnalyzedScan,
    output_directory: &Path,
    config: &SplitConfig,
    prefix: Option<&str>,
    preview_regions: Option<&[DetectedRegion]>,
) -> Result<SplitResult, SplitError> {
    config.validate()?;
    if let Some(prefix) = prefix {
        validate_prefix(prefix)?;
    }
    if photos.is_empty() {
        return Err(SplitError::NothingDetected);
    }
    if preview_regions.is_some_and(|regions| regions.len() != photos.len()) {
        return Err(SplitError::InvalidConfig(
            "Für jedes exportierte Foto muss genau eine Vorschauregion angegeben werden."
                .to_string(),
        ));
    }
    fs::create_dir_all(output_directory)?;
    let captured_at = capture_datetime(config.capture_date)?;
    let base = prefix
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("scan_{}", captured_at.format("%Y%m%d_%H%M%S")));
    let dpi = config.dpi.or(analyzed.embedded_dpi);
    let mut staged = Vec::with_capacity(photos.len() + usize::from(preview_regions.is_some()));
    for (index, photo) in photos.iter().enumerate() {
        let temporary = stage_image(
            photo,
            output_directory,
            config.output_format,
            config.jpeg_quality,
            dpi,
            captured_at,
        )?;
        staged.push(StagedOutput {
            temporary,
            stem: format!("{base}_{:02}", index + 1),
            extension: config.output_format.extension(),
        });
    }
    if let Some(regions) = preview_regions {
        staged.push(StagedOutput {
            temporary: stage_preview(&analyzed.image, regions, output_directory, captured_at)?,
            stem: format!("{base}_vorschau"),
            extension: "jpg",
        });
    }
    let mut published = publish_staged_group(output_directory, staged)?;
    let preview = preview_regions.map(|_| published.pop().expect("bereitgestellte Vorschau"));
    Ok(SplitResult {
        files: published,
        preview,
        threshold_used: analyzed.threshold,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    fn synthetic_scan() -> Mat {
        let mut image = Mat::new_rows_cols_with_default(
            1100,
            1500,
            core::CV_8UC3,
            Scalar::new(242.0, 246.0, 246.0, 0.0),
        )
        .unwrap();
        let rectangles = [
            RotatedRect::new(Point2f::new(340.0, 285.0), Size2f::new(420.0, 300.0), 7.0).unwrap(),
            RotatedRect::new(Point2f::new(1070.0, 275.0), Size2f::new(390.0, 280.0), -5.0).unwrap(),
            RotatedRect::new(Point2f::new(690.0, 790.0), Size2f::new(460.0, 320.0), 3.0).unwrap(),
        ];
        let colors = [
            Scalar::new(165.0, 110.0, 55.0, 0.0),
            Scalar::new(120.0, 55.0, 115.0, 0.0),
            Scalar::new(80.0, 135.0, 45.0, 0.0),
        ];
        for (rectangle, color) in rectangles.into_iter().zip(colors) {
            let mut points = [Point2f::default(); 4];
            rectangle.points(&mut points).unwrap();
            let polygon: Vector<Point> = points
                .iter()
                .map(|point| Point::new(point.x.round() as i32, point.y.round() as i32))
                .collect();
            imgproc::fill_convex_poly_def(&mut image, &polygon, color).unwrap();
        }
        image
    }

    fn plain_scan(width: i32, height: i32) -> Mat {
        Mat::new_rows_cols_with_default(
            height,
            width,
            core::CV_8UC3,
            Scalar::new(242.0, 246.0, 246.0, 0.0),
        )
        .unwrap()
    }

    fn filled_rectangle(image: &mut Mat, left: i32, top: i32, right: i32, bottom: i32) {
        let points: Vector<Point> = [
            Point::new(left, top),
            Point::new(right, top),
            Point::new(right, bottom),
            Point::new(left, bottom),
        ]
        .into_iter()
        .collect();
        imgproc::fill_convex_poly_def(image, &points, Scalar::new(165.0, 110.0, 55.0, 0.0))
            .unwrap();
    }

    fn staged_text(directory: &Path, stem: &str, contents: &str) -> StagedOutput {
        let mut temporary = TempFileBuilder::new()
            .prefix(".photoscanner-test-")
            .suffix(".txt")
            .tempfile_in(directory)
            .unwrap();
        temporary.write_all(contents.as_bytes()).unwrap();
        temporary.as_file().sync_all().unwrap();
        StagedOutput {
            temporary: temporary.into_temp_path(),
            stem: stem.to_string(),
            extension: "txt",
        }
    }

    #[test]
    fn detects_and_deskews_three_photos() {
        let (photos, threshold) =
            detect_photos(&synthetic_scan(), &SplitConfig::default()).unwrap();
        assert_eq!(photos.len(), 3);
        assert!(threshold >= 12.0);
        for photo in photos {
            assert!(photo.image.cols() > photo.image.rows());
            assert!(photo.image.cols() > 300);
            assert!(photo.image.rows() > 200);
        }
    }

    #[test]
    fn preserves_portrait_orientation_when_deskewing() {
        let image = plain_scan(500, 800);
        let portrait = warp_photo(
            &image,
            [
                Point2f::new(110.0, 90.0),
                Point2f::new(330.0, 90.0),
                Point2f::new(330.0, 710.0),
                Point2f::new(110.0, 710.0),
            ],
        )
        .unwrap();
        assert!(portrait.rows() > portrait.cols());
        assert_eq!(portrait.rows(), 620);
        assert_eq!(portrait.cols(), 220);
    }

    #[test]
    fn exports_all_formats_preview_and_photoprism_metadata() {
        let root = TempDir::new().unwrap();
        let source = root.path().join("scan.png");
        imgcodecs::imwrite_def(&source, &synthetic_scan()).unwrap();
        for format in [OutputFormat::Jpeg, OutputFormat::Png, OutputFormat::Tiff] {
            let config = SplitConfig {
                output_format: format,
                dpi: Some(600),
                capture_date: Some(NaiveDate::from_ymd_opt(1995, 9, 1).unwrap()),
                ..SplitConfig::default()
            };
            let result = split_scan(
                &source,
                &root.path().join(format.extension()),
                &config,
                Some("familie"),
                true,
            )
            .unwrap();
            assert_eq!(result.files.len(), 3);
            assert!(result.preview.as_ref().is_some_and(|path| path.is_file()));
            for path in result.files {
                assert!(path.is_file());
                let captured = crate::metadata::read_tag(&path, "Exif.Photo.DateTimeOriginal")
                    .unwrap()
                    .unwrap();
                assert!(captured.starts_with("1995:09:01"));
                assert_eq!(crate::metadata::image_dpi(&path).unwrap(), Some(600));
                if format != OutputFormat::Tiff {
                    assert_eq!(container_dpi(&path), Some(600));
                }
            }
        }
    }

    #[test]
    fn export_photos_preserves_supplied_rotation() {
        let root = TempDir::new().unwrap();
        let source = root.path().join("scan.png");
        imgcodecs::imwrite_def(&source, &synthetic_scan()).unwrap();
        let analyzed = analyze_scan(&source, &SplitConfig::default()).unwrap();
        let original = Mat::new_rows_cols_with_default(
            40,
            90,
            core::CV_8UC3,
            Scalar::new(20.0, 80.0, 160.0, 0.0),
        )
        .unwrap();
        let mut rotated = Mat::default();
        core::rotate(&original, &mut rotated, core::ROTATE_90_CLOCKWISE).unwrap();
        let config = SplitConfig {
            output_format: OutputFormat::Png,
            capture_date: NaiveDate::from_ymd_opt(1995, 9, 1),
            ..SplitConfig::default()
        };

        let result = export_photos(
            &[rotated],
            &analyzed,
            &root.path().join("rotated"),
            &config,
            Some("rotation"),
            None,
        )
        .unwrap();
        let exported = imgcodecs::imread(&result.files[0], imgcodecs::IMREAD_COLOR).unwrap();

        assert_eq!((exported.cols(), exported.rows()), (40, 90));
    }

    #[test]
    fn subset_export_is_contiguous_and_previews_only_included_regions() {
        let root = TempDir::new().unwrap();
        let source = root.path().join("scan.png");
        imgcodecs::imwrite_def(&source, &synthetic_scan()).unwrap();
        let config = SplitConfig {
            output_format: OutputFormat::Png,
            capture_date: NaiveDate::from_ymd_opt(1995, 9, 1),
            ..SplitConfig::default()
        };
        let analyzed = analyze_scan(&source, &config).unwrap();
        let regions = vec![analyzed.regions[0].clone(), analyzed.regions[2].clone()];
        let photos = regions
            .iter()
            .map(|region| warp_detected_photo(&analyzed, region).unwrap())
            .collect::<Vec<_>>();
        let output = root.path().join("subset");

        let result = export_photos(
            &photos,
            &analyzed,
            &output,
            &config,
            Some("auswahl"),
            Some(&regions),
        )
        .unwrap();

        assert_eq!(result.files.len(), 2);
        assert_eq!(
            result.files[0].file_name().unwrap().to_string_lossy(),
            "auswahl_01.png"
        );
        assert_eq!(
            result.files[1].file_name().unwrap().to_string_lossy(),
            "auswahl_02.png"
        );
        let expected_path = root.path().join("expected.jpg");
        save_preview(
            &analyzed.image,
            &regions,
            &expected_path,
            capture_datetime(config.capture_date).unwrap(),
        )
        .unwrap();
        let actual =
            imgcodecs::imread(result.preview.as_ref().unwrap(), imgcodecs::IMREAD_COLOR).unwrap();
        let expected = imgcodecs::imread(&expected_path, imgcodecs::IMREAD_COLOR).unwrap();
        assert_eq!(actual.data_bytes().unwrap(), expected.data_bytes().unwrap());
    }

    #[test]
    fn keeps_closely_spaced_photos_separate() {
        let mut image = plain_scan(1200, 700);
        filled_rectangle(&mut image, 90, 175, 570, 525);
        filled_rectangle(&mut image, 580, 175, 1060, 525);
        let (photos, _) = detect_photos(&image, &SplitConfig::default()).unwrap();
        assert_eq!(photos.len(), 2);
    }

    #[test]
    fn scanner_edge_does_not_join_adjacent_photos() {
        let mut image = plain_scan(1200, 1000);
        filled_rectangle(&mut image, 0, 0, 5, 999);
        filled_rectangle(&mut image, 3, 90, 363, 370);
        filled_rectangle(&mut image, 3, 610, 363, 890);
        let (photos, _) = detect_photos(&image, &SplitConfig::default()).unwrap();
        assert_eq!(photos.len(), 2);
    }

    #[test]
    fn does_not_overwrite_and_can_save_full_scan() {
        let root = TempDir::new().unwrap();
        let source = root.path().join("scan.png");
        imgcodecs::imwrite_def(&source, &synthetic_scan()).unwrap();
        let output = root.path().join("out");
        let config = SplitConfig {
            capture_date: Some(NaiveDate::from_ymd_opt(1995, 9, 1).unwrap()),
            ..SplitConfig::default()
        };
        let first = split_scan(&source, &output, &config, Some("foto"), false).unwrap();
        let second = split_scan(&source, &output, &config, Some("foto"), false).unwrap();
        let names: std::collections::HashSet<_> = first.files.iter().chain(&second.files).collect();
        assert_eq!(names.len(), 6);
        assert!(
            second.files.iter().all(|path| path
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .ends_with("_2"))
        );

        let full = save_full_scan(&source, &output, &config, Some("vollscan")).unwrap();
        let restored = imgcodecs::imread(&full, imgcodecs::IMREAD_COLOR).unwrap();
        assert_eq!(restored.cols(), 1500);
        assert_eq!(restored.rows(), 1100);

        let tiff_config = SplitConfig {
            output_format: OutputFormat::Tiff,
            ..config
        };
        let tiff = save_full_scan(&source, &output, &tiff_config, Some("vollscan-tiff")).unwrap();
        assert_eq!(image_dpi(&tiff).unwrap(), Some(72));
    }

    #[test]
    fn rejects_invalid_threshold() {
        let config = SplitConfig {
            threshold: Some(300.0),
            ..SplitConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_unsafe_prefixes_before_touching_the_output_directory() {
        let root = TempDir::new().unwrap();
        let missing_source = root.path().join("missing.png");
        let output = root.path().join("out");
        let absolute = root.path().join("absolute").to_string_lossy().into_owned();
        let too_long = "a".repeat(MAX_PREFIX_BYTES + 1);
        for prefix in [
            "",
            ".",
            "..",
            "../escaped",
            "unterordner/foto",
            "windows\\foto",
            "zeile\numbruch",
            &absolute,
            &too_long,
        ] {
            assert!(matches!(
                save_full_scan(
                    &missing_source,
                    &output,
                    &SplitConfig::default(),
                    Some(prefix)
                ),
                Err(SplitError::InvalidConfig(_))
            ));
        }
        assert!(!output.exists());
        assert!(!root.path().join("escaped.jpg").exists());
    }

    #[test]
    fn validates_detection_and_import_resource_limits() {
        assert!(validate_image_dimensions(10_200, 14_040).is_ok());
        assert!(validate_image_dimensions(20_000, 20_000).is_err());

        for maximum in [MIN_DETECTION_SIZE - 1, MAX_DETECTION_SIZE + 1] {
            let config = SplitConfig {
                max_detection_size: maximum,
                ..SplitConfig::default()
            };
            assert!(config.validate().is_err());
        }
        assert!(SplitConfig::default().validate().is_ok());
    }

    #[test]
    fn rejects_oversized_png_before_decoding_it() {
        let root = TempDir::new().unwrap();
        let source = root.path().join("oversized.png");
        let mut header = Vec::from(&b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"[..]);
        header.extend_from_slice(&20_000u32.to_be_bytes());
        header.extend_from_slice(&20_000u32.to_be_bytes());
        File::create(&source).unwrap().write_all(&header).unwrap();

        let error = save_full_scan(
            &source,
            &root.path().join("out"),
            &SplitConfig::default(),
            Some("gross"),
        )
        .unwrap_err();
        assert!(matches!(error, SplitError::InvalidConfig(message) if message.contains("zu groß")));
    }

    #[test]
    fn reads_dimensions_of_all_supported_input_formats() {
        let root = TempDir::new().unwrap();
        let image = plain_scan(80, 60);
        for extension in ["jpg", "png", "tif"] {
            let path = root.path().join(format!("bild.{extension}"));
            imgcodecs::imwrite_def(&path, &image).unwrap();
            assert_eq!(image_dimensions(&path).unwrap(), (80, 60));
        }
    }

    #[test]
    fn preview_is_bounded_instead_of_cloning_the_full_scan() {
        let root = TempDir::new().unwrap();
        let path = root.path().join("preview.jpg");
        save_preview(&plain_scan(3000, 2400), &[], &path, Local::now()).unwrap();
        let preview = imgcodecs::imread(&path, imgcodecs::IMREAD_COLOR).unwrap();
        assert_eq!(preview.cols(), MAX_PREVIEW_SIZE);
        assert_eq!(preview.rows(), 1600);
    }

    #[test]
    fn failed_group_commit_rolls_back_already_published_files() {
        let root = TempDir::new().unwrap();
        let staged = vec![
            staged_text(root.path(), "erstes", "eins"),
            staged_text(root.path(), "zweites", "zwei"),
        ];
        let first = root.path().join("erstes.txt");
        let destinations = vec![first.clone(), root.path().join("fehlt/zweites.txt")];
        assert!(commit_staged(&staged, &destinations).is_err());
        assert!(!first.exists());
        assert!(staged.iter().all(|output| output.temporary.is_file()));
    }

    #[test]
    #[ignore = "interner Worker für den Mehrprozess-Regressions-Test"]
    fn atomic_publish_process_worker() {
        let Some(directory) = std::env::var_os("PHOTOSCANNER_TEST_OUTPUT") else {
            return;
        };
        let id = std::env::var("PHOTOSCANNER_TEST_ID").unwrap();
        let directory = PathBuf::from(directory);
        let staged = staged_text(&directory, "parallel", &id);
        File::create(directory.join(format!("ready-{id}"))).unwrap();
        let started = Instant::now();
        while !directory.join("start").exists() {
            assert!(started.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(2));
        }
        publish_staged_group(&directory, vec![staged]).unwrap();
    }

    #[test]
    fn parallel_processes_never_clobber_an_existing_export() {
        const PROCESS_COUNT: usize = 8;
        let root = TempDir::new().unwrap();
        fs::write(root.path().join("parallel.txt"), "vorhanden").unwrap();
        let executable = std::env::current_exe().unwrap();
        let mut children = Vec::new();
        for id in 0..PROCESS_COUNT {
            children.push(
                Command::new(&executable)
                    .args([
                        "--exact",
                        "splitter::tests::atomic_publish_process_worker",
                        "--ignored",
                    ])
                    .env("PHOTOSCANNER_TEST_OUTPUT", root.path())
                    .env("PHOTOSCANNER_TEST_ID", id.to_string())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap(),
            );
        }
        let started = Instant::now();
        while (0..PROCESS_COUNT)
            .filter(|id| root.path().join(format!("ready-{id}")).exists())
            .count()
            != PROCESS_COUNT
        {
            assert!(started.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(2));
        }
        File::create(root.path().join("start")).unwrap();
        for mut child in children {
            assert!(child.wait().unwrap().success());
        }

        assert_eq!(
            fs::read_to_string(root.path().join("parallel.txt")).unwrap(),
            "vorhanden"
        );
        let exports: Vec<_> = fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|value| value == "txt"))
            .collect();
        assert_eq!(exports.len(), PROCESS_COUNT + 1);
        let contents: HashSet<_> = exports
            .iter()
            .map(|path| fs::read_to_string(path).unwrap())
            .collect();
        assert!(contents.contains("vorhanden"));
        for id in 0..PROCESS_COUNT {
            assert!(contents.contains(&id.to_string()));
        }
    }
}
