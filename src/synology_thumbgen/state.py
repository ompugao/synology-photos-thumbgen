from __future__ import annotations

import json
import os
import tempfile
import time
from pathlib import Path
from typing import Any


class StateStore:
    """Persistent JSON state store for DSM device token and per-unit backoff."""

    BASE_BACKOFF_SECONDS = 300
    MAX_BACKOFF_SECONDS = 86400

    def __init__(self, state_path: Path) -> None:
        self.path = state_path
        self.device_id: str | None = None
        self.failed_units: dict[str, dict[str, Any]] = {}
        self.load()

    def load(self) -> None:
        if not self.path.exists():
            return
        raw = json.loads(self.path.read_text(encoding="utf-8"))
        did = raw.get("device_id")
        self.device_id = str(did) if did else None
        failed = raw.get("failed_units")
        self.failed_units = dict(failed) if isinstance(failed, dict) else {}

    def save(self) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        payload = {
            "device_id": self.device_id,
            "failed_units": self.failed_units,
        }
        fd, tmp_name = tempfile.mkstemp(
            prefix=".state-",
            suffix=".tmp",
            dir=str(self.path.parent),
        )
        tmp_path = Path(tmp_name)
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as fh:
                json.dump(payload, fh, indent=2, sort_keys=True)
                fh.write("\n")
            os.chmod(tmp_path, 0o600)
            os.replace(tmp_path, self.path)
        except Exception:
            tmp_path.unlink(missing_ok=True)
            raise

    def set_device_id(self, device_id: str | None) -> None:
        normalized = device_id or None
        if normalized != self.device_id:
            self.device_id = normalized
            self.save()

    def record_failure(self, unit_id: int, error: str, now: float | None = None) -> float:
        ts = time.time() if now is None else now
        key = str(unit_id)
        prev = self.failed_units.get(key, {})
        attempts = int(prev.get("attempts", 0)) + 1
        delay = min(self.MAX_BACKOFF_SECONDS, self.BASE_BACKOFF_SECONDS * (2 ** (attempts - 1)))
        retry_after = ts + delay
        self.failed_units[key] = {
            "attempts": attempts,
            "last_error": error[:500],
            "retry_after": retry_after,
        }
        self.save()
        return retry_after

    def clear_failure(self, unit_id: int) -> None:
        key = str(unit_id)
        if key in self.failed_units:
            del self.failed_units[key]
            self.save()

    def should_skip(self, unit_id: int, now: float | None = None) -> bool:
        entry = self.failed_units.get(str(unit_id))
        if not entry:
            return False
        ts = time.time() if now is None else now
        retry_after = float(entry.get("retry_after", 0.0))
        return ts < retry_after
