from __future__ import annotations

import json
import logging
import os
import subprocess
from pathlib import Path
from typing import Any

from PIL import Image, ImageOps
import pillow_heif

from .config import Settings

logger = logging.getLogger(__name__)

IMAGE_EXTENSIONS = frozenset(
    {".heic", ".heif", ".jpg", ".jpeg", ".png", ".webp", ".tif", ".tiff", ".bmp", ".gif"}
)
VIDEO_EXTENSIONS = frozenset(
    {".mp4", ".mov", ".m4v", ".mkv", ".avi", ".mts", ".m2ts", ".wmv", ".3gp", ".webm"}
)
HDR_TRANSFERS = frozenset({"arib-std-b67", "smpte2084"})

_HEIF_REGISTERED = False


def ensure_heif_registered() -> None:
    global _HEIF_REGISTERED
    if not _HEIF_REGISTERED:
        pillow_heif.register_heif_opener()
        _HEIF_REGISTERED = True


def compute_thumbnail_dimensions(width: int, height: int) -> dict[str, tuple[int, int]]:
    """Compute Synology Photos thumb_xl / thumb_m / thumb_sm dimensions."""
    if width <= 0 or height <= 0:
        raise ValueError(f"Invalid image dimensions: {width}x{height}")

    scale_xl = min(1.0, 1280.0 / height)
    short_edge = min(width, height)
    scale_m = min(1.0, 320.0 / short_edge)
    scale_sm = min(1.0, 240.0 / short_edge)

    return {
        "thumb_xl": (max(1, round(width * scale_xl)), max(1, round(height * scale_xl))),
        "thumb_m": (max(1, round(width * scale_m)), max(1, round(height * scale_m))),
        "thumb_sm": (max(1, round(width * scale_sm)), max(1, round(height * scale_sm))),
    }


class LocalMediaResolver:
    """Resolve media unit paths under the mounted Photos root without HTTP download."""

    SKIP_DIRS = frozenset({"@eaDir", "#recycle", "#snapshot"})

    def __init__(self, photos_dir: Path) -> None:
        self.photos_dir = photos_dir
        self._filename_index: dict[str, list[Path]] | None = None

    def _build_index(self) -> dict[str, list[Path]]:
        index: dict[str, list[Path]] = {}
        if not self.photos_dir.exists():
            return index
        for root, dirnames, filenames in os.walk(self.photos_dir):
            dirnames[:] = [d for d in dirnames if d not in self.SKIP_DIRS]
            root_path = Path(root)
            for fn in filenames:
                index.setdefault(fn, []).append(root_path / fn)
        return index

    def resolve(self, filename: str, folder_rel_path: str | None = None) -> Path | None:
        if not filename or "/" in filename or "\\" in filename or filename in {".", ".."}:
            return None

        root_resolved = self.photos_dir.resolve()

        if folder_rel_path is not None:
            rel_parts = [p for p in Path(folder_rel_path.lstrip("/")).parts if p not in {"", "."}]
            candidate = (root_resolved.joinpath(*rel_parts, filename)).resolve()
            if candidate.is_relative_to(root_resolved) and candidate.is_file():
                return candidate

        if self._filename_index is None:
            self._filename_index = self._build_index()

        matches = self._filename_index.get(filename)
        if not matches:
            self._filename_index = self._build_index()
            matches = self._filename_index.get(filename)

        if matches and len(matches) == 1 and matches[0].is_file():
            return matches[0]
        return None


def probe_video(src_path: Path, ffprobe_bin: str = "ffprobe") -> dict[str, Any]:
    """Probe video stream metadata using ffprobe."""
    cmd = [
        ffprobe_bin,
        "-v",
        "error",
        "-select_streams",
        "v:0",
        "-show_entries",
        "stream=width,height,codec_name,pix_fmt,color_transfer:format=duration",
        "-of",
        "json",
        str(src_path),
    ]
    proc = subprocess.run(cmd, capture_output=True, text=True, check=False)
    if proc.returncode != 0:
        raise RuntimeError(f"ffprobe failed for {src_path}: {proc.stderr.strip() or proc.stdout.strip()}")

    data = json.loads(proc.stdout or "{}")
    streams = data.get("streams") or []
    if not streams:
        raise RuntimeError(f"No video stream found in {src_path}")

    stream = streams[0]
    raw_duration = (data.get("format") or {}).get("duration")
    try:
        duration = float(raw_duration) if raw_duration is not None else 0.0
    except (TypeError, ValueError):
        duration = 0.0

    return {
        "width": int(stream.get("width") or 0),
        "height": int(stream.get("height") or 0),
        "codec_name": str(stream.get("codec_name") or ""),
        "pix_fmt": str(stream.get("pix_fmt") or ""),
        "color_transfer": str(stream.get("color_transfer") or ""),
        "duration": duration,
    }


def _build_video_filter_complex(use_hdr_tonemap: bool) -> str:
    if use_hdr_tonemap:
        pre_filter = (
            "zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,"
            "tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv,format=yuvj420p"
        )
    else:
        pre_filter = "format=yuvj420p"

    return (
        f"[0:v]{pre_filter},split=3[s_xl][s_m][s_sm];"
        "[s_xl]scale=-2:'min(1280\\,ih)'[xl];"
        "[s_m]scale='if(gt(iw\\,ih)\\,-2\\,min(320\\,iw))':'if(gt(iw\\,ih)\\,min(320\\,ih)\\,-2)'[m];"
        "[s_sm]scale='if(gt(iw\\,ih)\\,-2\\,min(240\\,iw))':'if(gt(iw\\,ih)\\,min(240\\,ih)\\,-2)'[sm]"
    )


def _run_ffmpeg_thumbs(
    src_path: Path,
    out_paths: tuple[Path, Path, Path],
    ffmpeg_bin: str,
    seek_seconds: float | None,
    use_hdr_tonemap: bool,
) -> subprocess.CompletedProcess[str]:
    thumb_xl, thumb_m, thumb_sm = out_paths
    filter_complex = _build_video_filter_complex(use_hdr_tonemap=use_hdr_tonemap)

    cmd: list[str] = [ffmpeg_bin, "-hide_banner", "-v", "error", "-y"]
    if seek_seconds is not None and seek_seconds > 0:
        cmd.extend(["-noaccurate_seek", "-ss", f"{seek_seconds:.3f}"])
    cmd.extend(["-i", str(src_path), "-filter_complex", filter_complex])

    for label, target in (("[xl]", thumb_xl), ("[m]", thumb_m), ("[sm]", thumb_sm)):
        cmd.extend(
            [
                "-map",
                label,
                "-frames:v",
                "1",
                "-update",
                "1",
                "-q:v",
                "3",
                "-strict",
                "unofficial",
                str(target),
            ]
        )

    return subprocess.run(cmd, capture_output=True, text=True, check=False)


def _outputs_valid(paths: tuple[Path, Path, Path]) -> bool:
    return all(p.is_file() and p.stat().st_size > 0 for p in paths)


def generate_video_thumbnails(
    src_path: Path,
    out_dir: Path,
    ffmpeg_bin: str = "ffmpeg",
    ffprobe_bin: str = "ffprobe",
) -> tuple[Path, Path, Path]:
    """Generate thumb_sm, thumb_m, thumb_xl JPEGs for a video in a single decode pass."""
    out_dir.mkdir(parents=True, exist_ok=True)
    thumb_sm = out_dir / "thumb_sm.jpg"
    thumb_m = out_dir / "thumb_m.jpg"
    thumb_xl = out_dir / "thumb_xl.jpg"
    out_paths = (thumb_xl, thumb_m, thumb_sm)

    meta = probe_video(src_path, ffprobe_bin=ffprobe_bin)
    duration = float(meta.get("duration") or 0.0)
    is_hdr = str(meta.get("color_transfer") or "") in HDR_TRANSFERS
    initial_seek: float | None = 1.0 if duration >= 1.5 else None

    attempts: list[tuple[float | None, bool]] = [(initial_seek, is_hdr)]
    if is_hdr:
        attempts.append((initial_seek, False))
    if initial_seek is not None:
        attempts.append((None, False))

    last_error = ""
    for seek, hdr in attempts:
        for p in out_paths:
            p.unlink(missing_ok=True)
        proc = _run_ffmpeg_thumbs(
            src_path=src_path,
            out_paths=out_paths,
            ffmpeg_bin=ffmpeg_bin,
            seek_seconds=seek,
            use_hdr_tonemap=hdr,
        )
        if proc.returncode == 0 and _outputs_valid(out_paths):
            return thumb_sm, thumb_m, thumb_xl
        last_error = proc.stderr.strip() or proc.stdout.strip() or f"exit code {proc.returncode}"
        logger.debug(
            "ffmpeg attempt failed (seek=%s hdr=%s) for %s: %s",
            seek,
            hdr,
            src_path,
            last_error,
        )

    raise RuntimeError(f"ffmpeg failed to extract thumbnails for {src_path}: {last_error}")


def generate_image_thumbnails(
    src_path: Path,
    out_dir: Path,
    jpeg_quality: int = 90,
) -> tuple[Path, Path, Path]:
    """Generate thumb_sm, thumb_m, thumb_xl JPEGs for an image (including HEIC/HEIF)."""
    ensure_heif_registered()
    out_dir.mkdir(parents=True, exist_ok=True)
    thumb_sm = out_dir / "thumb_sm.jpg"
    thumb_m = out_dir / "thumb_m.jpg"
    thumb_xl = out_dir / "thumb_xl.jpg"

    with Image.open(src_path) as opened:
        transposed = ImageOps.exif_transpose(opened)
        rgb = transposed.convert("RGB")
        dims = compute_thumbnail_dimensions(*rgb.size)

        for key, dest in (
            ("thumb_xl", thumb_xl),
            ("thumb_m", thumb_m),
            ("thumb_sm", thumb_sm),
        ):
            target_size = dims[key]
            resized = (
                rgb
                if rgb.size == target_size
                else rgb.resize(target_size, Image.Resampling.LANCZOS)
            )
            resized.save(dest, format="JPEG", quality=jpeg_quality, optimize=True)

    return thumb_sm, thumb_m, thumb_xl


def generate_thumbnails(
    src_path: Path,
    unit_type: int | None,
    out_dir: Path,
    settings: Settings,
) -> tuple[Path, Path, Path]:
    """Dispatch photo vs video thumbnail generation."""
    ext = src_path.suffix.lower()
    if unit_type == 0 or (unit_type is None and ext in IMAGE_EXTENSIONS):
        return generate_image_thumbnails(
            src_path=src_path,
            out_dir=out_dir,
            jpeg_quality=settings.jpeg_quality,
        )
    if unit_type in (1, 2) or (unit_type is None and ext in VIDEO_EXTENSIONS):
        return generate_video_thumbnails(
            src_path=src_path,
            out_dir=out_dir,
            ffmpeg_bin=settings.ffmpeg_bin,
            ffprobe_bin=settings.ffprobe_bin,
        )
    raise ValueError(f"Unsupported media file type (unit_type={unit_type}, ext={ext}): {src_path}")
