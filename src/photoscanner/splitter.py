"""Erkennung, Begradigung und Export einzelner Fotos auf einem Scanbett."""

from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from typing import Iterable

import cv2
import numpy as np
from PIL import Image, ImageOps


class SplitError(RuntimeError):
    """Fehler beim Laden, Erkennen oder Speichern eines Scans."""


@dataclass(frozen=True, slots=True)
class SplitConfig:
    min_area_percent: float = 2.0
    threshold: float | None = None
    padding_percent: float = 1.2
    max_detection_size: int = 1800
    output_format: str = "jpg"
    jpeg_quality: int = 95
    dpi: int | None = None

    def validate(self) -> None:
        if not 0.1 <= self.min_area_percent <= 50:
            raise SplitError("Die Mindestfläche muss zwischen 0,1 und 50 Prozent liegen.")
        if self.threshold is not None and not 1 <= self.threshold <= 255:
            raise SplitError("Der Schwellwert muss zwischen 1 und 255 liegen.")
        if not 0 <= self.padding_percent <= 15:
            raise SplitError("Der Rand muss zwischen 0 und 15 Prozent liegen.")
        if self.output_format.lower() not in {"jpg", "jpeg", "png", "tif", "tiff"}:
            raise SplitError("Unterstützte Formate sind JPG, PNG und TIFF.")
        if not 1 <= self.jpeg_quality <= 100:
            raise SplitError("Die JPEG-Qualität muss zwischen 1 und 100 liegen.")


@dataclass(slots=True)
class DetectedPhoto:
    image: np.ndarray
    center: tuple[float, float]
    source_box: np.ndarray
    area_percent: float


@dataclass(frozen=True, slots=True)
class SplitResult:
    files: tuple[Path, ...]
    preview: Path | None
    threshold_used: float


def _read_image(path: Path) -> tuple[np.ndarray, tuple[float, float] | None]:
    try:
        with Image.open(path) as source:
            dpi = source.info.get("dpi")
            rgb = np.asarray(ImageOps.exif_transpose(source).convert("RGB"))
    except (OSError, ValueError) as exc:
        raise SplitError(f"Bild konnte nicht geöffnet werden: {path}: {exc}") from exc
    if rgb.ndim != 3 or min(rgb.shape[:2]) < 20:
        raise SplitError("Das Scanbild ist zu klein oder hat ein ungültiges Format.")
    normalized_dpi: tuple[float, float] | None = None
    if isinstance(dpi, tuple) and len(dpi) >= 2:
        normalized_dpi = (float(dpi[0]), float(dpi[1]))
    return cv2.cvtColor(rgb, cv2.COLOR_RGB2BGR), normalized_dpi


def _scaled_for_detection(image: np.ndarray, maximum: int) -> tuple[np.ndarray, float]:
    height, width = image.shape[:2]
    scale = min(1.0, maximum / max(height, width))
    if scale == 1.0:
        return image.copy(), scale
    resized = cv2.resize(
        image,
        (max(1, round(width * scale)), max(1, round(height * scale))),
        interpolation=cv2.INTER_AREA,
    )
    return resized, scale


def _background_samples(image: np.ndarray) -> np.ndarray:
    height, width = image.shape[:2]
    strip = max(2, round(min(height, width) * 0.015))
    return np.concatenate(
        (
            image[:strip].reshape(-1, 3),
            image[-strip:].reshape(-1, 3),
            image[:, :strip].reshape(-1, 3),
            image[:, -strip:].reshape(-1, 3),
        ),
        axis=0,
    )


def _foreground_mask(
    image: np.ndarray, threshold_override: float | None
) -> tuple[np.ndarray, float]:
    blurred = cv2.GaussianBlur(image, (5, 5), 0)
    lab = cv2.cvtColor(blurred, cv2.COLOR_BGR2LAB).astype(np.float32)
    edge_samples = _background_samples(lab)
    background = np.median(edge_samples, axis=0)
    border_distance = np.linalg.norm(edge_samples - background, axis=1)
    distance = np.linalg.norm(lab - background, axis=2)

    if threshold_override is None:
        noise = float(np.percentile(border_distance, 95))
        threshold = float(np.clip(max(12.0, noise * 2.5), 12.0, 48.0))
    else:
        threshold = float(threshold_override)

    mask = (distance >= threshold).astype(np.uint8) * 255
    short_side = min(image.shape[:2])
    close_size = max(3, round(short_side * 0.012))
    close_size += 1 - close_size % 2
    open_size = max(3, round(short_side * 0.0025))
    open_size += 1 - open_size % 2
    close_kernel = cv2.getStructuringElement(cv2.MORPH_ELLIPSE, (close_size, close_size))
    open_kernel = cv2.getStructuringElement(cv2.MORPH_ELLIPSE, (open_size, open_size))
    mask = cv2.morphologyEx(mask, cv2.MORPH_CLOSE, close_kernel, iterations=2)
    mask = cv2.morphologyEx(mask, cv2.MORPH_OPEN, open_kernel, iterations=1)

    # Ein dunkler Rand der Scannerfläche darf Fotos, die links oder oben am
    # Glasrand liegen, nicht zu einer einzigen Komponente verbinden. Das
    # Abschneiden weniger Pixel trennt diesen Rahmen, ohne relevante Bildfläche
    # eines randnahen Fotos zu verlieren.
    border = max(3, round(short_side * 0.01))
    mask[:border, :] = 0
    mask[-border:, :] = 0
    mask[:, :border] = 0
    mask[:, -border:] = 0
    return mask, threshold


def _order_box(points: np.ndarray) -> np.ndarray:
    points = points.astype(np.float32)
    ordered = np.zeros((4, 2), dtype=np.float32)
    coordinate_sum = points.sum(axis=1)
    coordinate_difference = np.diff(points, axis=1).reshape(-1)
    ordered[0] = points[np.argmin(coordinate_sum)]  # oben links
    ordered[2] = points[np.argmax(coordinate_sum)]  # unten rechts
    ordered[1] = points[np.argmin(coordinate_difference)]  # oben rechts
    ordered[3] = points[np.argmax(coordinate_difference)]  # unten links
    return ordered


def _expand_rectangle(
    rectangle: tuple[tuple[float, float], tuple[float, float], float], percent: float
) -> tuple[tuple[float, float], tuple[float, float], float]:
    center, (width, height), angle = rectangle
    factor = 1.0 + (2.0 * percent / 100.0)
    return center, (width * factor, height * factor), angle


def _warp_photo(image: np.ndarray, box: np.ndarray) -> np.ndarray:
    ordered = _order_box(box)
    top_width = np.linalg.norm(ordered[1] - ordered[0])
    bottom_width = np.linalg.norm(ordered[2] - ordered[3])
    left_height = np.linalg.norm(ordered[3] - ordered[0])
    right_height = np.linalg.norm(ordered[2] - ordered[1])
    width = max(1, round(max(top_width, bottom_width)))
    height = max(1, round(max(left_height, right_height)))
    destination = np.array(
        [[0, 0], [width - 1, 0], [width - 1, height - 1], [0, height - 1]],
        dtype=np.float32,
    )
    transform = cv2.getPerspectiveTransform(ordered, destination)
    warped = cv2.warpPerspective(
        image,
        transform,
        (width, height),
        flags=cv2.INTER_CUBIC,
        borderMode=cv2.BORDER_REPLICATE,
    )
    # Übliche Fotoausrichtung: lange Seite waagerecht.
    if warped.shape[0] > warped.shape[1]:
        warped = cv2.rotate(warped, cv2.ROTATE_90_CLOCKWISE)
    return warped


def detect_photos(
    image: np.ndarray, config: SplitConfig
) -> tuple[list[DetectedPhoto], float]:
    """Erkennt Fotos in einem BGR-Scanbild und gibt entzerrte Ausschnitte zurück."""
    config.validate()
    detection_image, scale = _scaled_for_detection(image, config.max_detection_size)
    mask, threshold = _foreground_mask(detection_image, config.threshold)
    contours, _ = cv2.findContours(mask, cv2.RETR_EXTERNAL, cv2.CHAIN_APPROX_SIMPLE)
    scan_area = float(mask.shape[0] * mask.shape[1])
    minimum_area = scan_area * config.min_area_percent / 100.0
    detected: list[DetectedPhoto] = []

    for contour in contours:
        area = float(cv2.contourArea(contour))
        if area < minimum_area or area > scan_area * 0.97:
            continue
        rectangle = cv2.minAreaRect(contour)
        width, height = rectangle[1]
        if min(width, height) < max(12, min(mask.shape[:2]) * 0.04):
            continue
        rectangle = _expand_rectangle(rectangle, config.padding_percent)
        small_box = cv2.boxPoints(rectangle)
        full_box = small_box / scale
        full_box[:, 0] = np.clip(full_box[:, 0], 0, image.shape[1] - 1)
        full_box[:, 1] = np.clip(full_box[:, 1], 0, image.shape[0] - 1)
        photo = _warp_photo(image, full_box)
        if min(photo.shape[:2]) < 10:
            continue
        center = (rectangle[0][0] / scale, rectangle[0][1] / scale)
        detected.append(
            DetectedPhoto(
                image=photo,
                center=center,
                source_box=full_box,
                area_percent=area / scan_area * 100.0,
            )
        )

    if detected:
        median_height = float(np.median([np.ptp(item.source_box[:, 1]) for item in detected]))
        row_height = max(1.0, median_height * 0.5)
        detected.sort(key=lambda item: (round(item.center[1] / row_height), item.center[0]))
    return detected, threshold


def _unique_path(directory: Path, stem: str, suffix: str) -> Path:
    candidate = directory / f"{stem}{suffix}"
    counter = 2
    while candidate.exists():
        candidate = directory / f"{stem}_{counter}{suffix}"
        counter += 1
    return candidate


def _save_photo(
    image: np.ndarray,
    path: Path,
    *,
    quality: int,
    dpi: tuple[float, float] | None,
) -> None:
    rgb = cv2.cvtColor(image, cv2.COLOR_BGR2RGB)
    output = Image.fromarray(rgb)
    options: dict[str, object] = {}
    suffix = path.suffix.lower()
    if suffix in {".jpg", ".jpeg"}:
        options.update(quality=quality, subsampling=0, optimize=True)
    elif suffix in {".tif", ".tiff"}:
        options["compression"] = "tiff_lzw"
    if dpi:
        options["dpi"] = dpi
    try:
        output.save(path, **options)
    except OSError as exc:
        raise SplitError(f"Foto konnte nicht gespeichert werden: {path}: {exc}") from exc


def _save_preview(image: np.ndarray, photos: Iterable[DetectedPhoto], path: Path) -> None:
    overlay = image.copy()
    font_scale = max(0.7, min(image.shape[:2]) / 1400)
    thickness = max(2, round(font_scale * 2))
    for index, photo in enumerate(photos, start=1):
        polygon = np.rint(photo.source_box).astype(np.int32)
        cv2.polylines(overlay, [polygon], True, (30, 190, 40), thickness)
        anchor = tuple(polygon[np.argmin(polygon[:, 0] + polygon[:, 1])])
        cv2.putText(
            overlay,
            str(index),
            (int(anchor[0]) + 8, int(anchor[1]) + 28),
            cv2.FONT_HERSHEY_SIMPLEX,
            font_scale,
            (20, 40, 230),
            thickness,
            cv2.LINE_AA,
        )
    rgb = cv2.cvtColor(overlay, cv2.COLOR_BGR2RGB)
    Image.fromarray(rgb).save(path, quality=88, optimize=True)


def split_scan(
    source: Path,
    output_directory: Path,
    config: SplitConfig | None = None,
    *,
    prefix: str | None = None,
    save_preview: bool = True,
) -> SplitResult:
    """Teilt einen Scan auf und speichert die erkannten Einzelfotos."""
    config = config or SplitConfig()
    config.validate()
    source = source.expanduser().resolve()
    if not source.is_file():
        raise SplitError(f"Die Eingabedatei existiert nicht: {source}")
    image, embedded_dpi = _read_image(source)
    photos, threshold = detect_photos(image, config)
    if not photos:
        raise SplitError(
            "Keine einzelnen Fotos erkannt. Lege zwischen den Fotos Abstand frei oder "
            "versuche einen kleineren Schwellwert mit --threshold."
        )

    output_directory = output_directory.expanduser().resolve()
    output_directory.mkdir(parents=True, exist_ok=True)
    normalized_format = config.output_format.lower().replace("jpeg", "jpg").replace("tiff", "tif")
    suffix = f".{normalized_format}"
    base = prefix or f"scan_{datetime.now():%Y%m%d_%H%M%S}"
    effective_dpi = (
        (float(config.dpi), float(config.dpi)) if config.dpi is not None else embedded_dpi
    )

    files: list[Path] = []
    for index, photo in enumerate(photos, start=1):
        path = _unique_path(output_directory, f"{base}_{index:02d}", suffix)
        _save_photo(
            photo.image,
            path,
            quality=config.jpeg_quality,
            dpi=effective_dpi,
        )
        files.append(path)

    preview_path: Path | None = None
    if save_preview:
        preview_path = _unique_path(output_directory, f"{base}_vorschau", ".jpg")
        _save_preview(image, photos, preview_path)
    return SplitResult(tuple(files), preview_path, threshold)
