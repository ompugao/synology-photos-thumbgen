from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path


def _parse_bool(value: str | None, default: bool) -> bool:
    if value is None or value.strip() == "":
        return default
    return value.strip().lower() in {"1", "true", "yes", "on"}


def _parse_int(value: str | None, default: int, minimum: int = 1) -> int:
    if value is None or value.strip() == "":
        return default
    parsed = int(value.strip())
    if parsed < minimum:
        raise ValueError(f"Expected integer >= {minimum}, got {parsed}")
    return parsed


@dataclass(frozen=True, slots=True)
class Settings:
    """Runtime configuration loaded from environment variables."""

    dsm_url: str = "http://127.0.0.1:5000"
    dsm_username: str = ""
    dsm_password: str = ""
    photos_dir: Path = Path("/photos")
    data_dir: Path = Path("/data")
    poll_interval_seconds: int = 1800
    watch_enabled: bool = True
    watch_debounce_seconds: int = 15
    watch_followup_seconds: int = 45
    verify_ssl: bool = True
    mark_broken_on_error: bool = False
    download_fallback: bool = True
    ffmpeg_bin: str = "ffmpeg"
    ffprobe_bin: str = "ffprobe"
    jpeg_quality: int = 90
    log_level: str = "INFO"
    device_name: str = "SynologyPhotosThumbGen"

    @classmethod
    def from_env(cls, env: dict[str, str] | None = None) -> Settings:
        src = env if env is not None else os.environ
        dsm_url = (src.get("DSM_URL") or "http://127.0.0.1:5000").strip().rstrip("/")
        watch_enabled = _parse_bool(src.get("WATCH_ENABLED"), True)
        default_poll = 1800 if watch_enabled else 300
        return cls(
            dsm_url=dsm_url,
            dsm_username=(src.get("DSM_USERNAME") or "").strip(),
            dsm_password=src.get("DSM_PASSWORD") or "",
            photos_dir=Path(src.get("PHOTOS_DIR") or "/photos"),
            data_dir=Path(src.get("DATA_DIR") or "/data"),
            poll_interval_seconds=_parse_int(src.get("POLL_INTERVAL_SECONDS"), default_poll, minimum=1),
            watch_enabled=watch_enabled,
            watch_debounce_seconds=_parse_int(src.get("WATCH_DEBOUNCE_SECONDS"), 15, minimum=1),
            watch_followup_seconds=_parse_int(src.get("WATCH_FOLLOWUP_SECONDS"), 45, minimum=0),
            verify_ssl=_parse_bool(src.get("DSM_VERIFY_SSL"), True),
            mark_broken_on_error=_parse_bool(src.get("MARK_BROKEN_ON_ERROR"), False),
            download_fallback=_parse_bool(src.get("ALLOW_DOWNLOAD_FALLBACK"), True),
            ffmpeg_bin=(src.get("FFMPEG_BIN") or "ffmpeg").strip(),
            ffprobe_bin=(src.get("FFPROBE_BIN") or "ffprobe").strip(),
            jpeg_quality=_parse_int(src.get("JPEG_QUALITY"), 90, minimum=1),
            log_level=(src.get("LOG_LEVEL") or "INFO").strip().upper(),
            device_name=(src.get("DSM_DEVICE_NAME") or "SynologyPhotosThumbGen").strip(),
        )
