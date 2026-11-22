from __future__ import annotations

import argparse
import getpass
import logging
import signal
import sys
import tempfile
import threading
import time
from dataclasses import replace
from pathlib import Path
from typing import Any

from PIL import Image

from .config import Settings
from .dsm_client import DsmApiError, DsmAuthFatalError, DsmPhotosClient, DsmSessionExpiredError
from .media import LocalMediaResolver, generate_thumbnails
from .state import StateStore
from .watcher import MediaInotifyWatcher

logger = logging.getLogger("synology_thumbgen")


def configure_logging(level: str) -> None:
    logging.basicConfig(
        level=getattr(logging, level.upper(), logging.INFO),
        format="%(asctime)s [%(levelname)s] %(name)s: %(message)s",
        datefmt="%Y-%m-%d %H:%M:%S",
    )


def _validate_credentials(settings: Settings) -> bool:
    if not settings.dsm_username or not settings.dsm_password:
        logger.error(
            "DSM_USERNAME or DSM_PASSWORD is empty. "
            "Set DSM_USERNAME and DSM_PASSWORD in .env before running."
        )
        return False
    return True


def _resolve_or_download(
    client: DsmPhotosClient,
    resolver: LocalMediaResolver,
    item: dict[str, Any],
    tmp_dir: Path,
    allow_download_fallback: bool,
) -> tuple[Path, str]:
    unit_id = int(item["unit_id"])
    filename = str(item["filename"])
    folder_rel = client.resolve_folder_path(unit_id)
    local_path = resolver.resolve(filename=filename, folder_rel_path=folder_rel)
    if local_path is not None:
        return local_path, "local"

    if not allow_download_fallback:
        raise FileNotFoundError(
            f"Local file not found for unit_id={unit_id} filename={filename} "
            f"(folder={folder_rel!r}) and ALLOW_DOWNLOAD_FALLBACK=false"
        )

    download_path = tmp_dir / filename
    logger.info("Local file not matched for %s (%s); downloading via API", filename, unit_id)
    client.download_unit(unit_id, download_path)
    return download_path, "download"


def run_cycle(
    client: DsmPhotosClient,
    resolver: LocalMediaResolver,
    state: StateStore,
    settings: Settings,
    dry_run: bool = False,
    limit: int | None = None,
) -> dict[str, int]:
    """Process one cycle of pending thumbnails until queue is drained or limit reached."""
    stats = {"processed": 0, "succeeded": 0, "failed": 0, "skipped": 0}
    seen_ids: set[int] = set()
    had_transient_failure = False

    while limit is None or stats["processed"] < limit:
        pending = client.list_convert_needed()
        batch = [item for item in pending if int(item["unit_id"]) not in seen_ids]
        if not batch:
            break

        for item in batch:
            if limit is not None and stats["processed"] >= limit:
                break

            unit_id = int(item["unit_id"])
            seen_ids.add(unit_id)

            if state.should_skip(unit_id):
                stats["skipped"] += 1
                logger.debug("Skipping unit_id=%s (%s) in backoff", unit_id, item.get("filename"))
                continue

            filename = str(item.get("filename") or f"unit_{unit_id}")
            unit_type = int(item.get("type", 1))
            need_thumb = bool(item.get("need_thumbnail", True))
            need_video = bool(item.get("need_video", False))

            if dry_run:
                folder_rel = client.resolve_folder_path(unit_id)
                local_path = resolver.resolve(filename=filename, folder_rel_path=folder_rel)
                source_desc = str(local_path) if local_path else "<download-fallback>"
                logger.info(
                    "[DRY-RUN] unit_id=%s type=%s need_thumbnail=%s need_video=%s filename=%s source=%s",
                    unit_id,
                    unit_type,
                    need_thumb,
                    need_video,
                    filename,
                    source_desc,
                )
                stats["processed"] += 1
                stats["succeeded"] += 1
                continue

            stats["processed"] += 1
            t0 = time.monotonic()
            try:
                with tempfile.TemporaryDirectory(prefix=f"thumb_{unit_id}_") as tmp_raw:
                    tmp_dir = Path(tmp_raw)
                    src_path, source_kind = _resolve_or_download(
                        client=client,
                        resolver=resolver,
                        item=item,
                        tmp_dir=tmp_dir,
                        allow_download_fallback=settings.download_fallback,
                    )
                    out_dir = tmp_dir / "out"
                    thumb_sm, thumb_m, thumb_xl = generate_thumbnails(
                        src_path=src_path,
                        unit_type=unit_type,
                        out_dir=out_dir,
                        settings=settings,
                    )
                    client.upload_thumbnails(
                        unit_id=unit_id,
                        thumb_sm=thumb_sm,
                        thumb_m=thumb_m,
                        thumb_xl=thumb_xl,
                    )
                    elapsed_ms = int((time.monotonic() - t0) * 1000)
                    logger.info(
                        "Uploaded thumbnails for %s (unit_id=%s, source=%s) in %d ms "
                        "[sm=%dB m=%dB xl=%dB]",
                        filename,
                        unit_id,
                        source_kind,
                        elapsed_ms,
                        thumb_sm.stat().st_size,
                        thumb_m.stat().st_size,
                        thumb_xl.stat().st_size,
                    )
                state.clear_failure(unit_id)
                stats["succeeded"] += 1
            except DsmSessionExpiredError:
                raise
            except Exception as exc:
                stats["failed"] += 1
                had_transient_failure = True
                retry_after = state.record_failure(unit_id, str(exc))
                logger.warning(
                    "Failed unit_id=%s (%s): %s (next retry in %ds)",
                    unit_id,
                    filename,
                    exc,
                    max(0, int(retry_after - time.time())),
                )
                if settings.mark_broken_on_error:
                    try:
                        client.set_broken(unit_id, unit_type)
                    except Exception as broken_exc:
                        logger.warning("Failed to call set_broken on unit_id=%s: %s", unit_id, broken_exc)

    if had_transient_failure and not dry_run:
        # Refresh session after failures so server-side allocation entries clear for retryable items.
        client.logout()

    return stats


def _run_cycle_with_reauth(
    client: DsmPhotosClient,
    resolver: LocalMediaResolver,
    state: StateStore,
    settings: Settings,
    dry_run: bool = False,
    limit: int | None = None,
) -> dict[str, int]:
    if not client.sid:
        client.login()
    try:
        return run_cycle(
            client=client,
            resolver=resolver,
            state=state,
            settings=settings,
            dry_run=dry_run,
            limit=limit,
        )
    except DsmSessionExpiredError:
        logger.info("DSM session expired; re-authenticating once")
        client.sid = None
        client.syno_token = None
        client.login()
        return run_cycle(
            client=client,
            resolver=resolver,
            state=state,
            settings=settings,
            dry_run=dry_run,
            limit=limit,
        )


def cmd_once(settings: Settings, dry_run: bool, limit: int | None) -> int:
    if not _validate_credentials(settings):
        return 2

    state = StateStore(settings.data_dir / "state.json")
    resolver = LocalMediaResolver(settings.photos_dir)

    with DsmPhotosClient(settings=settings, state=state) as client:
        try:
            client.login()
            stats = _run_cycle_with_reauth(
                client=client,
                resolver=resolver,
                state=state,
                settings=settings,
                dry_run=dry_run,
                limit=limit,
            )
            logger.info(
                "Cycle complete: processed=%d succeeded=%d failed=%d skipped=%d",
                stats["processed"],
                stats["succeeded"],
                stats["failed"],
                stats["skipped"],
            )
            return 0
        except DsmAuthFatalError as exc:
            logger.error("%s", exc)
            if exc.requires_otp:
                logger.error("2FA is enabled. Run `docker compose run --rm thumbgen auth` once.")
            return 2
        finally:
            client.logout()


def cmd_daemon(settings: Settings) -> int:
    if not _validate_credentials(settings):
        return 2

    state = StateStore(settings.data_dir / "state.json")
    resolver = LocalMediaResolver(settings.photos_dir)
    stop_event = threading.Event()
    wake_event = threading.Event()
    event_lock = threading.Lock()
    pending_event_at: float | None = None
    last_event_log_at = 0.0
    suppressed_event_count = 0
    followup_due_at: float | None = None

    def _handle_signal(signum: int, _: object) -> None:
        logger.info("Received signal %s; shutting down", signum)
        stop_event.set()
        wake_event.set()

    def _on_media_event(path: Path | None, reason: str) -> None:
        nonlocal pending_event_at, last_event_log_at, suppressed_event_count, followup_due_at
        now_mono = time.monotonic()
        should_log = False
        prev_suppressed = 0
        with event_lock:
            if pending_event_at is None:
                pending_event_at = now_mono
            followup_due_at = None
            if now_mono - last_event_log_at >= settings.watch_debounce_seconds:
                should_log = True
                last_event_log_at = now_mono
                prev_suppressed = suppressed_event_count
                suppressed_event_count = 0
            else:
                suppressed_event_count += 1
        if should_log:
            target = str(path) if path is not None else "<overflow>"
            if prev_suppressed > 0:
                logger.info(
                    "Detected media change (%s): %s (+%d more in previous window)",
                    reason,
                    target,
                    prev_suppressed,
                )
            else:
                logger.info("Detected media change (%s): %s", reason, target)
        wake_event.set()

    signal.signal(signal.SIGINT, _handle_signal)
    signal.signal(signal.SIGTERM, _handle_signal)

    with DsmPhotosClient(settings=settings, state=state) as client:
        try:
            client.login()
        except DsmAuthFatalError as exc:
            logger.error("%s", exc)
            if exc.requires_otp:
                logger.error("2FA is enabled. Run `docker compose run --rm thumbgen auth` once.")
            return 2

        watcher: MediaInotifyWatcher | None = None
        if settings.watch_enabled and settings.photos_dir.exists():
            try:
                watcher = MediaInotifyWatcher(settings.photos_dir, _on_media_event)
                watch_count = watcher.start()
                logger.info(
                    "Inotify watcher started (%d directories under %s, debounce=%ds, followup=%ds, fallback_poll=%ds)",
                    watch_count,
                    settings.photos_dir,
                    settings.watch_debounce_seconds,
                    settings.watch_followup_seconds,
                    settings.poll_interval_seconds,
                )
            except OSError as exc:
                logger.warning(
                    "Failed to start inotify watcher on %s (%s); falling back to periodic polling",
                    settings.photos_dir,
                    exc,
                )
                watcher = None
        elif settings.watch_enabled:
            logger.warning("PHOTOS_DIR does not exist (%s); inotify watcher disabled", settings.photos_dir)

        logger.info(
            "Daemon started (poll_interval=%ds, photos_dir=%s, watch_enabled=%s)",
            settings.poll_interval_seconds,
            settings.photos_dir,
            settings.watch_enabled,
        )
        next_fallback_poll_at = 0.0
        initial_cycle_pending = True
        try:
            while not stop_event.is_set():
                now = time.monotonic()
                with event_lock:
                    event_due = (
                        pending_event_at + settings.watch_debounce_seconds
                        if pending_event_at is not None
                        else None
                    )
                    followup_due = followup_due_at

                candidates: list[tuple[float, str]] = [
                    (next_fallback_poll_at, "startup" if initial_cycle_pending else "fallback_poll")
                ]
                if event_due is not None:
                    candidates.append((event_due, "inotify"))
                if followup_due is not None:
                    candidates.append((followup_due, "followup"))

                next_due, trigger = min(candidates, key=lambda item: item[0])
                if now < next_due:
                    wake_event.wait(timeout=next_due - now)
                    wake_event.clear()
                    continue

                with event_lock:
                    if trigger == "inotify":
                        pending_event_at = None
                        suppressed_event_count = 0
                        followup_due_at = None
                    elif trigger == "followup":
                        followup_due_at = None
                    else:
                        pending_event_at = None
                        suppressed_event_count = 0
                        followup_due_at = None

                initial_cycle_pending = False
                try:
                    stats = _run_cycle_with_reauth(
                        client=client,
                        resolver=resolver,
                        state=state,
                        settings=settings,
                    )
                    if stats["processed"] > 0 or stats["skipped"] > 0 or trigger != "fallback_poll":
                        logger.info(
                            "Cycle complete (%s): processed=%d succeeded=%d failed=%d skipped=%d",
                            trigger,
                            stats["processed"],
                            stats["succeeded"],
                            stats["failed"],
                            stats["skipped"],
                        )
                    next_fallback_poll_at = time.monotonic() + settings.poll_interval_seconds
                    if (
                        trigger == "inotify"
                        and stats["processed"] == 0
                        and settings.watch_followup_seconds > 0
                    ):
                        with event_lock:
                            if pending_event_at is None:
                                followup_due_at = time.monotonic() + settings.watch_followup_seconds
                                logger.info(
                                    "No pending items yet after media event; scheduling follow-up check in %ds",
                                    settings.watch_followup_seconds,
                                )
                except DsmAuthFatalError as exc:
                    logger.error("Stopping daemon to prevent DSM Auto Block: %s", exc)
                    if exc.requires_otp:
                        logger.error("Run `docker compose run --rm thumbgen auth` to refresh device token.")
                    return 2
                except (DsmApiError, Exception) as exc:
                    logger.error("Poll cycle error (will retry next interval): %s", exc)
                    next_fallback_poll_at = time.monotonic() + min(settings.poll_interval_seconds, 300)
        finally:
            if watcher is not None:
                watcher.stop()
            client.logout()
    return 0


def cmd_auth(settings: Settings) -> int:
    if not settings.dsm_username:
        logger.error("DSM_USERNAME is empty.")
        return 2
    password = settings.dsm_password or getpass.getpass(f"DSM password for {settings.dsm_username}: ")
    if not password:
        logger.error("DSM password cannot be empty.")
        return 2

    auth_settings = replace(settings, dsm_password=password)
    state = StateStore(auth_settings.data_dir / "state.json")

    with DsmPhotosClient(settings=auth_settings, state=state) as client:
        try:
            try:
                did = client.login()
            except DsmAuthFatalError as exc:
                if not exc.requires_otp:
                    logger.error("%s", exc)
                    return 2
                otp_code = input("2FA OTP code: ").strip()
                if not otp_code:
                    logger.error("OTP code is required.")
                    return 2
                did = client.login(otp_code=otp_code)

            pending = client.list_convert_needed()
            logger.info(
                "Authentication succeeded (device_id saved=%s). Pending thumbnails visible now: %d",
                bool(did),
                len(pending),
            )
            return 0
        except DsmAuthFatalError as exc:
            logger.error("%s", exc)
            return 2
        finally:
            client.logout()


def cmd_test_local(settings: Settings, file_path: Path, out_dir: Path | None) -> int:
    if not file_path.is_file():
        logger.error("File not found: %s", file_path)
        return 1

    def _run_into(dest_dir: Path) -> int:
        t0 = time.monotonic()
        try:
            thumb_sm, thumb_m, thumb_xl = generate_thumbnails(
                src_path=file_path,
                unit_type=None,
                out_dir=dest_dir,
                settings=settings,
            )
        except Exception as exc:
            logger.error("Thumbnail generation failed for %s: %s", file_path, exc)
            return 1
        elapsed_ms = int((time.monotonic() - t0) * 1000)

        for label, path in (("thumb_xl", thumb_xl), ("thumb_m", thumb_m), ("thumb_sm", thumb_sm)):
            with Image.open(path) as img:
                img.verify()
            with Image.open(path) as img:
                w, h = img.size
            print(f"{label}: {w}x{h} ({path.stat().st_size} bytes) -> {path}")
        print(f"elapsed_ms: {elapsed_ms}")
        return 0

    if out_dir is not None:
        return _run_into(out_dir)

    with tempfile.TemporaryDirectory(prefix="thumbgen_test_") as tmp_raw:
        return _run_into(Path(tmp_raw))


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="synology-thumbgen",
        description="Generate Synology Photos thumbnails using ffmpeg and SYNO.Foto.Upload.ConvertedFile",
    )
    sub = parser.add_subparsers(dest="command")

    sub.add_parser("daemon", help="Poll DSM periodically and upload missing thumbnails (default)")

    once_parser = sub.add_parser("once", help="Run a single polling cycle and exit")
    once_parser.add_argument("--dry-run", action="store_true", help="List pending items without uploading")
    once_parser.add_argument("--limit", type=int, default=None, help="Maximum number of items to process")

    sub.add_parser("auth", help="Interactive login to store 2FA device_id in state.json")

    test_parser = sub.add_parser("test-local", help="Offline thumbnail extraction check on a local file")
    test_parser.add_argument("file", type=Path, help="Path to local video or HEIC/image file")
    test_parser.add_argument("--out-dir", type=Path, default=None, help="Optional output directory")

    return parser


def main(argv: list[str] | None = None) -> int:
    args_list = list(sys.argv[1:] if argv is None else argv)
    if not args_list:
        args_list = ["daemon"]

    parser = build_parser()
    args = parser.parse_args(args_list)

    settings = Settings.from_env()
    configure_logging(settings.log_level)

    if args.command in (None, "daemon"):
        return cmd_daemon(settings)
    if args.command == "once":
        return cmd_once(settings, dry_run=args.dry_run, limit=args.limit)
    if args.command == "auth":
        return cmd_auth(settings)
    if args.command == "test-local":
        return cmd_test_local(settings, file_path=args.file, out_dir=args.out_dir)

    parser.print_help()
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
