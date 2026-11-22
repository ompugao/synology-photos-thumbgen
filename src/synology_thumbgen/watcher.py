from __future__ import annotations

import ctypes
import errno
import logging
import os
import select
import struct
import threading
from pathlib import Path
from typing import Callable

from .media import IMAGE_EXTENSIONS, VIDEO_EXTENSIONS

logger = logging.getLogger(__name__)

IN_CLOSE_WRITE = 0x00000008
IN_CREATE = 0x00000100
IN_MOVED_TO = 0x00000080
IN_DELETE_SELF = 0x00000400
IN_MOVE_SELF = 0x00000800
IN_Q_OVERFLOW = 0x00004000
IN_IGNORED = 0x00008000
IN_ISDIR = 0x40000000

IN_NONBLOCK = 0o0004000
IN_CLOEXEC = 0o2000000

WATCH_MASK = IN_CLOSE_WRITE | IN_MOVED_TO | IN_CREATE | IN_DELETE_SELF | IN_MOVE_SELF
SKIP_DIRS = frozenset({"@eaDir", "#recycle", "#snapshot"})
MEDIA_EXTENSIONS = IMAGE_EXTENSIONS | VIDEO_EXTENSIONS

_EVENT_HEADER = struct.Struct("iIII")


def is_watched_media_file(name: str) -> bool:
    """Return True if filename is a supported media file and not hidden/metadata."""
    if not name or name.startswith(".") or "@SynoEAStream" in name:
        return False
    return Path(name).suffix.lower() in MEDIA_EXTENSIONS


class MediaInotifyWatcher:
    """Pure-Python Linux inotify directory tree watcher for media additions."""

    def __init__(
        self,
        root_dir: Path,
        on_media_event: Callable[[Path | None, str], None],
    ) -> None:
        self.root_dir = root_dir
        self.on_media_event = on_media_event
        self._libc = self._load_libc()
        self._fd: int = -1
        self._wake_r: int = -1
        self._wake_w: int = -1
        self._stop_event = threading.Event()
        self._thread: threading.Thread | None = None
        self.wd_to_path: dict[int, Path] = {}
        self.path_to_wd: dict[Path, int] = {}

    @staticmethod
    def _load_libc() -> ctypes.CDLL:
        libc = ctypes.CDLL("libc.so.6", use_errno=True)
        libc.inotify_init1.argtypes = [ctypes.c_int]
        libc.inotify_init1.restype = ctypes.c_int
        libc.inotify_add_watch.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_uint32]
        libc.inotify_add_watch.restype = ctypes.c_int
        libc.inotify_rm_watch.argtypes = [ctypes.c_int, ctypes.c_int]
        libc.inotify_rm_watch.restype = ctypes.c_int
        return libc

    def __enter__(self) -> MediaInotifyWatcher:
        self.start()
        return self

    def __exit__(self, *_: object) -> None:
        self.stop()

    def start(self) -> int:
        if self._thread is not None:
            return len(self.wd_to_path)

        fd = self._libc.inotify_init1(IN_NONBLOCK | IN_CLOEXEC)
        if fd < 0:
            err = ctypes.get_errno()
            raise OSError(err, os.strerror(err))

        wake_r, wake_w = os.pipe()
        os.set_blocking(wake_r, False)
        os.set_blocking(wake_w, False)

        self._fd = fd
        self._wake_r = wake_r
        self._wake_w = wake_w
        self._stop_event.clear()

        try:
            self._add_tree(self.root_dir, notify_existing=False)
        except Exception:
            self.stop()
            raise

        self._thread = threading.Thread(
            target=self._run_loop,
            name="synology-thumbgen-inotify",
            daemon=True,
        )
        self._thread.start()
        return len(self.wd_to_path)

    def _add_watch(self, path: Path) -> bool:
        if path in self.path_to_wd:
            return False
        raw_path = os.fsencode(path)
        wd = self._libc.inotify_add_watch(self._fd, raw_path, WATCH_MASK)
        if wd < 0:
            err = ctypes.get_errno()
            if err in (errno.ENOENT, errno.ENOTDIR, errno.EACCES):
                logger.debug("Skipping inotify watch for %s: %s", path, os.strerror(err))
                return False
            if err == errno.ENOSPC:
                logger.warning(
                    "fs.inotify.max_user_watches reached while watching %s; "
                    "new directories may rely on fallback polling",
                    path,
                )
                return False
            logger.warning("inotify_add_watch failed for %s: %s", path, os.strerror(err))
            return False

        old_path = self.wd_to_path.get(wd)
        if old_path is not None and old_path != path:
            self.path_to_wd.pop(old_path, None)
        self.wd_to_path[wd] = path
        self.path_to_wd[path] = wd
        return True

    def _drop_wd(self, wd: int) -> None:
        path = self.wd_to_path.pop(wd, None)
        if path is not None and self.path_to_wd.get(path) == wd:
            self.path_to_wd.pop(path, None)

    def _add_tree(self, base: Path, notify_existing: bool) -> None:
        if not base.exists() or base.name in SKIP_DIRS:
            return
        try:
            for dirpath, dirnames, filenames in os.walk(base):
                dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
                dir_path = Path(dirpath)
                self._add_watch(dir_path)
                if notify_existing:
                    for fn in filenames:
                        if is_watched_media_file(fn):
                            self.on_media_event(dir_path / fn, "new_dir_scan")
        except OSError as exc:
            logger.debug("Failed walking %s for inotify watches: %s", base, exc)

    def _handle_raw_buffer(self, buf: bytes) -> None:
        offset = 0
        total = len(buf)
        header_size = _EVENT_HEADER.size
        while offset + header_size <= total:
            wd, mask, _cookie, length = _EVENT_HEADER.unpack_from(buf, offset)
            offset += header_size
            raw_name = buf[offset : offset + length]
            offset += length
            name = raw_name.split(b"\x00", 1)[0].decode("utf-8", errors="surrogateescape")

            if mask & IN_Q_OVERFLOW:
                logger.warning("inotify event queue overflow; scheduling sync check")
                self.on_media_event(None, "overflow")
                continue

            if mask & (IN_IGNORED | IN_DELETE_SELF):
                self._drop_wd(wd)
                continue

            parent = self.wd_to_path.get(wd)
            if parent is None:
                continue

            if mask & IN_ISDIR:
                if (mask & (IN_CREATE | IN_MOVED_TO)) and name and name not in SKIP_DIRS:
                    self._add_tree(parent / name, notify_existing=True)
            else:
                if (mask & (IN_CLOSE_WRITE | IN_MOVED_TO)) and is_watched_media_file(name):
                    reason = "close_write" if (mask & IN_CLOSE_WRITE) else "moved_to"
                    self.on_media_event(parent / name, reason)

    def _run_loop(self) -> None:
        while not self._stop_event.is_set():
            try:
                readable, _, _ = select.select([self._fd, self._wake_r], [], [])
            except (OSError, ValueError):
                break

            if self._wake_r in readable or self._stop_event.is_set():
                break

            if self._fd in readable:
                while not self._stop_event.is_set():
                    try:
                        chunk = os.read(self._fd, 65536)
                    except BlockingIOError:
                        break
                    except OSError:
                        return
                    if not chunk:
                        break
                    self._handle_raw_buffer(chunk)

    def stop(self) -> None:
        self._stop_event.set()
        if self._wake_w >= 0:
            try:
                os.write(self._wake_w, b"x")
            except OSError:
                pass
        if self._thread is not None and self._thread.is_alive():
            self._thread.join(timeout=5.0)
        self._thread = None

        for fd_attr in ("_fd", "_wake_r", "_wake_w"):
            fd = getattr(self, fd_attr)
            if fd >= 0:
                try:
                    os.close(fd)
                except OSError:
                    pass
                setattr(self, fd_attr, -1)

        self.wd_to_path.clear()
        self.path_to_wd.clear()
