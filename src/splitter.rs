use std::cmp::Ordering;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono::{DateTime, Local, NaiveDate, TimeZone, Timelike};
use filetime::{FileTime, set_file_mtime};
use little_exif::exif_tag::ExifTag;
use little_exif::metadata::Metadata;
use little_exif::rational::uR64;
use opencv::core::{
    self, CV_8UC1, Mat, Point, Point2f, RotatedRect, Scalar, Size, Size2f, Vec3b, Vector,
};
use opencv::geometry;
use opencv::imgcodecs;
use opencv::imgproc;
use opencv::prelude::*;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SplitError {
    #[error("{0}")]
    InvalidConfig(String),
    #[error("Die Eingabedatei existiert nicht: {0}")]
    MissingSource(PathBuf),
    #[error("Bild konnte nicht verarbeitet werden: {0}")]
    OpenCv(#[from] opencv::Error),
    #[error("Dateioperation fehlgeschlagen: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "Keine einzelnen Fotos erkannt. Lege zwischen den Fotos Abstand frei oder versuche einen kleineren Schwellwert."
    )]
    NothingDetected,
    #[error("Das ausgewählte Aufnahmedatum ist in der lokalen Zeitzone ungültig.")]
    InvalidDate,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputFormat {
    #[default]
    Jpeg,
    Png,
    Tiff,
}

impl OutputFormat {
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
pub struct SplitConfig {
    pub min_area_percent: f64,
    pub threshold: Option<f64>,
    pub padding_percent: f64,
    pub max_detection_size: i32,
    pub output_format: OutputFormat,
    pub jpeg_quality: i32,
    pub dpi: Option<u32>,
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
        if !(1..=100).contains(&self.jpeg_quality) {
            return Err(SplitError::InvalidConfig(
                "Die JPEG-Qualität muss zwischen 1 und 100 liegen.".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct DetectedPhoto {
    pub image: Mat,
    pub center: Point2f,
    pub source_box: [Point2f; 4],
    pub area_percent: f64,
}

#[derive(Clone, Debug)]
pub struct SplitResult {
    pub files: Vec<PathBuf>,
    pub preview: Option<PathBuf>,
    pub threshold_used: f64,
}

fn image_orientation(path: &Path) -> Option<u16> {
    let metadata = Metadata::new_from_path(path).ok()?;
    metadata
        .get_tag_by_hex(0x0112, None)
        .find_map(|tag| match tag {
            ExifTag::Orientation(values) => values.first().copied(),
            _ => None,
        })
}

fn image_dpi(path: &Path) -> Option<u32> {
    Metadata::new_from_path(path)
        .ok()
        .and_then(|metadata| {
            metadata
                .get_tag_by_hex(0x011a, None)
                .find_map(|tag| match tag {
                    ExifTag::XResolution(values) => values.first().and_then(|value| {
                        (value.denominator != 0).then_some(
                            (value.nominator as f64 / value.denominator as f64).round() as u32,
                        )
                    }),
                    _ => None,
                })
        })
        .or_else(|| container_dpi(path))
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
    let mut image = imgcodecs::imread(path, imgcodecs::IMREAD_COLOR)?;
    if image.empty() || image.rows() < 20 || image.cols() < 20 {
        return Err(SplitError::InvalidConfig(
            "Das Scanbild ist zu klein oder hat ein ungültiges Format.".to_string(),
        ));
    }
    if let Some(orientation) = image_orientation(path) {
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
            _ => return Ok((image, image_dpi(path))),
        }
        image = transformed;
    }
    Ok((image, image_dpi(path)))
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
    if warped.rows() > warped.cols() {
        let mut rotated = Mat::default();
        core::rotate(&warped, &mut rotated, core::ROTATE_90_CLOCKWISE)?;
        warped = rotated;
    }
    Ok(warped)
}

pub fn detect_photos(
    image: &Mat,
    config: &SplitConfig,
) -> Result<(Vec<DetectedPhoto>, f64), SplitError> {
    config.validate()?;
    let (detection_image, scale) = scaled_for_detection(image, config.max_detection_size)?;
    let (mask, threshold) = foreground_mask(&detection_image, config.threshold)?;
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
        let photo = warp_photo(image, source_box)?;
        if photo.rows().min(photo.cols()) < 10 {
            continue;
        }
        detected.push(DetectedPhoto {
            image: photo,
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

fn unique_path(directory: &Path, stem: &str, extension: &str) -> PathBuf {
    let initial = directory.join(format!("{stem}.{extension}"));
    if !initial.exists() {
        return initial;
    }
    for counter in 2.. {
        let candidate = directory.join(format!("{stem}_{counter}.{extension}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
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

fn write_metadata(
    path: &Path,
    captured_at: DateTime<Local>,
    dpi: Option<u32>,
) -> Result<(), SplitError> {
    let mut metadata = Metadata::new_from_path(path).unwrap_or_else(|_| Metadata::new());
    let timestamp = captured_at.format("%Y:%m:%d %H:%M:%S").to_string();
    let offset = captured_at.format("%:z").to_string();
    metadata.set_tag(ExifTag::ModifyDate(timestamp.clone()));
    metadata.set_tag(ExifTag::DateTimeOriginal(timestamp.clone()));
    metadata.set_tag(ExifTag::CreateDate(timestamp));
    metadata.set_tag(ExifTag::Software("Photo Scanner".to_string()));
    metadata.set_tag(ExifTag::OffsetTime(offset.clone()));
    metadata.set_tag(ExifTag::OffsetTimeOriginal(offset.clone()));
    metadata.set_tag(ExifTag::OffsetTimeDigitized(offset));
    if let Some(dpi) = dpi {
        let resolution = vec![uR64 {
            nominator: dpi,
            denominator: 1,
        }];
        metadata.set_tag(ExifTag::XResolution(resolution.clone()));
        metadata.set_tag(ExifTag::YResolution(resolution));
        metadata.set_tag(ExifTag::ResolutionUnit(vec![2]));
    }
    metadata.write_to_file(path)?;
    set_file_mtime(
        path,
        FileTime::from_unix_time(
            captured_at.timestamp(),
            captured_at.timestamp_subsec_nanos(),
        ),
    )?;
    Ok(())
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
    if let Err(error) = write_metadata(path, captured_at, metadata_dpi) {
        let _ = fs::remove_file(path);
        return Err(error);
    }
    Ok(())
}

fn save_preview(
    image: &Mat,
    photos: &[DetectedPhoto],
    path: &Path,
    captured_at: DateTime<Local>,
) -> Result<(), SplitError> {
    let mut overlay = image.try_clone()?;
    let font_scale = (image.rows().min(image.cols()) as f64 / 1400.0).clamp(0.7, 4.0);
    let thickness = (font_scale * 2.0).round().max(2.0) as i32;
    for (index, photo) in photos.iter().enumerate() {
        let polygon: Vector<Point> = photo
            .source_box
            .iter()
            .map(|point| Point::new(point.x.round() as i32, point.y.round() as i32))
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

pub fn save_full_scan(
    source: &Path,
    output_directory: &Path,
    config: &SplitConfig,
    prefix: Option<&str>,
) -> Result<PathBuf, SplitError> {
    config.validate()?;
    let (image, embedded_dpi) = read_image(source)?;
    fs::create_dir_all(output_directory)?;
    let captured_at = capture_datetime(config.capture_date)?;
    let base = prefix
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("scan_{}", captured_at.format("%Y%m%d_%H%M%S")));
    let path = unique_path(output_directory, &base, config.output_format.extension());
    save_image(
        &image,
        &path,
        config.output_format,
        config.jpeg_quality,
        config.dpi.or(embedded_dpi),
        captured_at,
    )?;
    Ok(path)
}

pub fn split_scan(
    source: &Path,
    output_directory: &Path,
    config: &SplitConfig,
    prefix: Option<&str>,
    create_preview: bool,
) -> Result<SplitResult, SplitError> {
    config.validate()?;
    let (image, embedded_dpi) = read_image(source)?;
    let (photos, threshold) = detect_photos(&image, config)?;
    if photos.is_empty() {
        return Err(SplitError::NothingDetected);
    }
    fs::create_dir_all(output_directory)?;
    let captured_at = capture_datetime(config.capture_date)?;
    let base = prefix
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("scan_{}", captured_at.format("%Y%m%d_%H%M%S")));
    let dpi = config.dpi.or(embedded_dpi);
    let mut files = Vec::with_capacity(photos.len());
    for (index, photo) in photos.iter().enumerate() {
        let path = unique_path(
            output_directory,
            &format!("{base}_{:02}", index + 1),
            config.output_format.extension(),
        );
        save_image(
            &photo.image,
            &path,
            config.output_format,
            config.jpeg_quality,
            dpi,
            captured_at,
        )?;
        files.push(path);
    }
    let preview = if create_preview {
        let path = unique_path(output_directory, &format!("{base}_vorschau"), "jpg");
        save_preview(&image, &photos, &path, captured_at)?;
        Some(path)
    } else {
        None
    };
    Ok(SplitResult {
        files,
        preview,
        threshold_used: threshold,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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
                let metadata = Metadata::new_from_path(&path).unwrap();
                assert!(metadata.get_tag_by_hex(0x9003, None).any(|tag| {
                    matches!(tag, ExifTag::DateTimeOriginal(value) if value.starts_with("1995:09:01"))
                }));
                assert!(metadata.get_tag_by_hex(0x011a, None).any(|tag| {
                    matches!(tag, ExifTag::XResolution(values) if values.first().is_some_and(|value| value.nominator == 600))
                }));
                if format != OutputFormat::Tiff {
                    assert_eq!(container_dpi(&path), Some(600));
                }
            }
        }
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

        let full = save_full_scan(&source, &output, &config, Some("vollscan")).unwrap();
        let restored = imgcodecs::imread(&full, imgcodecs::IMREAD_COLOR).unwrap();
        assert_eq!(restored.cols(), 1500);
        assert_eq!(restored.rows(), 1100);

        let tiff_config = SplitConfig {
            output_format: OutputFormat::Tiff,
            ..config
        };
        let tiff = save_full_scan(&source, &output, &tiff_config, Some("vollscan-tiff")).unwrap();
        assert_eq!(image_dpi(&tiff), Some(72));
    }

    #[test]
    fn rejects_invalid_threshold() {
        let config = SplitConfig {
            threshold: Some(300.0),
            ..SplitConfig::default()
        };
        assert!(config.validate().is_err());
    }
}
