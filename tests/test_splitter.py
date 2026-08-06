from __future__ import annotations

from datetime import datetime
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest

import cv2
import numpy as np
from PIL import ExifTags, Image, ImageDraw

from photoscanner.splitter import SplitConfig, SplitError, detect_photos, split_scan


def make_photo(size: tuple[int, int], color: tuple[int, int, int]) -> Image.Image:
    photo = Image.new("RGB", size, color)
    draw = ImageDraw.Draw(photo)
    width, height = size
    draw.rectangle((18, 18, width - 18, height - 18), outline="white", width=8)
    draw.ellipse((width * 0.2, height * 0.2, width * 0.7, height * 0.8), fill=(230, 180, 50))
    draw.line((30, height - 40, width - 30, 40), fill=(20, 40, 70), width=15)
    return photo


def paste_rotated(canvas: Image.Image, photo: Image.Image, center: tuple[int, int], angle: float) -> None:
    rotated = photo.rotate(angle, expand=True, resample=Image.Resampling.BICUBIC, fillcolor=(246, 246, 242))
    position = (center[0] - rotated.width // 2, center[1] - rotated.height // 2)
    canvas.paste(rotated, position)


def synthetic_scan() -> Image.Image:
    canvas = Image.new("RGB", (1500, 1100), (246, 246, 242))
    paste_rotated(canvas, make_photo((420, 300), (55, 110, 165)), (340, 285), 7)
    paste_rotated(canvas, make_photo((390, 280), (115, 55, 120)), (1070, 275), -5)
    paste_rotated(canvas, make_photo((460, 320), (45, 135, 80)), (690, 790), 3)
    return canvas


def edge_touching_scan() -> Image.Image:
    canvas = Image.new("RGB", (1200, 1000), (246, 246, 242))
    # Simuliert einen dunklen Rand der Scannerfläche, der zwei randnahe Fotos
    # andernfalls optisch miteinander verbinden würde.
    ImageDraw.Draw(canvas).rectangle((0, 0, 5, 999), fill=(30, 30, 30))
    first = make_photo((360, 280), (55, 110, 165))
    second = make_photo((360, 280), (115, 55, 120))
    canvas.paste(first, (3, 90))
    canvas.paste(second, (3, 610))
    return canvas


class SplitterTests(unittest.TestCase):
    def test_detects_and_deskews_three_photos(self) -> None:
        rgb = np.asarray(synthetic_scan())
        bgr = cv2.cvtColor(rgb, cv2.COLOR_RGB2BGR)
        photos, threshold = detect_photos(bgr, SplitConfig(min_area_percent=2.0))
        self.assertEqual(3, len(photos))
        self.assertGreaterEqual(threshold, 12)
        for photo in photos:
            height, width = photo.image.shape[:2]
            self.assertGreater(width, height)
            self.assertGreater(width, 300)
            self.assertGreater(height, 200)

    def test_splits_and_writes_preview_and_dpi(self) -> None:
        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "scan.png"
            synthetic_scan().save(source, dpi=(600, 600))
            result = split_scan(
                source,
                root / "out",
                SplitConfig(output_format="png"),
                prefix="familie",
            )
            self.assertEqual(3, len(result.files))
            self.assertIsNotNone(result.preview)
            self.assertTrue(result.preview.is_file())
            for path in result.files:
                self.assertTrue(path.is_file())
                with Image.open(path) as image:
                    self.assertGreater(image.width, image.height)
                    self.assertAlmostEqual(600, image.info["dpi"][0], delta=1)

    def test_exports_current_photoprism_date_metadata(self) -> None:
        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "scan.png"
            synthetic_scan().save(source)
            today = datetime.now().astimezone().strftime("%Y:%m:%d")

            for output_format in ("jpg", "png", "tif"):
                result = split_scan(
                    source,
                    root / output_format,
                    SplitConfig(output_format=output_format),
                    prefix="archiv",
                )
                paths = (*result.files, result.preview)
                for path in paths:
                    self.assertIsNotNone(path)
                    with Image.open(path) as image:
                        exif = image.getexif()
                        self.assertTrue(exif[ExifTags.Base.DateTimeOriginal].startswith(today))
                        self.assertTrue(exif[ExifTags.Base.DateTimeDigitized].startswith(today))
                        self.assertTrue(exif[ExifTags.Base.DateTime].startswith(today))
                        self.assertEqual("Photo Scanner", exif[ExifTags.Base.Software])
                    modified = datetime.fromtimestamp(path.stat().st_mtime).astimezone()
                    self.assertEqual(datetime.now().astimezone().date(), modified.date())

    def test_scanner_edge_does_not_join_adjacent_photos(self) -> None:
        rgb = np.asarray(edge_touching_scan())
        bgr = cv2.cvtColor(rgb, cv2.COLOR_RGB2BGR)
        photos, _ = detect_photos(bgr, SplitConfig(min_area_percent=2.0))
        self.assertEqual(2, len(photos))

    def test_does_not_overwrite_existing_files(self) -> None:
        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "scan.jpg"
            synthetic_scan().save(source)
            first = split_scan(source, root / "out", prefix="foto", save_preview=False)
            second = split_scan(source, root / "out", prefix="foto", save_preview=False)
            self.assertEqual(6, len(set(first.files + second.files)))

    def test_rejects_invalid_threshold(self) -> None:
        with self.assertRaises(SplitError):
            SplitConfig(threshold=300).validate()


if __name__ == "__main__":
    unittest.main()
