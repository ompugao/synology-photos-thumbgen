from __future__ import annotations

import json
import threading
from pathlib import Path

import httpx
import pytest
from PIL import Image

from synology_thumbgen.cli import cmd_auth
from synology_thumbgen.config import Settings
from synology_thumbgen.dsm_client import (
    DsmAuthFatalError,
    DsmPhotosClient,
)
from synology_thumbgen.media import LocalMediaResolver, compute_thumbnail_dimensions, generate_image_thumbnails
from synology_thumbgen.state import StateStore
from synology_thumbgen.watcher import MediaInotifyWatcher


def test_compute_thumbnail_dimensions_landscape_4k() -> None:
    dims = compute_thumbnail_dimensions(3840, 2160)
    assert dims["thumb_xl"] == (2276, 1280)
    assert dims["thumb_m"] == (569, 320)
    assert dims["thumb_sm"] == (427, 240)


def test_compute_thumbnail_dimensions_portrait() -> None:
    dims = compute_thumbnail_dimensions(1512, 2688)
    assert dims["thumb_xl"] == (720, 1280)
    assert dims["thumb_m"] == (320, 569)
    assert dims["thumb_sm"] == (240, 427)


def test_compute_thumbnail_dimensions_small_no_upscale() -> None:
    dims = compute_thumbnail_dimensions(200, 100)
    assert dims["thumb_xl"] == (200, 100)
    assert dims["thumb_m"] == (200, 100)
    assert dims["thumb_sm"] == (200, 100)


def test_state_store_persistence_and_backoff(tmp_path: Path) -> None:
    state_path = tmp_path / "state.json"
    store = StateStore(state_path)
    store.set_device_id("dev-123")
    retry1 = store.record_failure(42, "decode error", now=1000.0)
    assert retry1 == 1300.0
    assert store.should_skip(42, now=1200.0) is True
    assert store.should_skip(42, now=1300.0) is False

    retry2 = store.record_failure(42, "decode error", now=1300.0)
    assert retry2 == 1900.0

    reloaded = StateStore(state_path)
    assert reloaded.device_id == "dev-123"
    assert reloaded.failed_units["42"]["attempts"] == 2

    reloaded.clear_failure(42)
    assert reloaded.should_skip(42, now=1301.0) is False


def test_local_media_resolver_exact_traversal_unique_and_duplicate(tmp_path: Path) -> None:
    photos = tmp_path / "Photos"
    dir_a = photos / "CameraA" / "202501"
    dir_b = photos / "CameraB" / "202501"
    eadir = dir_a / "@eaDir" / "clip.mp4"
    dir_a.mkdir(parents=True)
    dir_b.mkdir(parents=True)
    eadir.mkdir(parents=True)

    file_a = dir_a / "unique.mp4"
    file_a.write_bytes(b"a")
    (eadir / "SYNOPHOTO_THUMB_XL.jpg").write_bytes(b"ignore")

    (dir_a / "dup.mp4").write_bytes(b"d1")
    (dir_b / "dup.mp4").write_bytes(b"d2")

    resolver = LocalMediaResolver(photos)
    # Exact folder resolution
    assert resolver.resolve("dup.mp4", "/CameraB/202501") == (dir_b / "dup.mp4").resolve()
    # Path traversal blocked
    assert resolver.resolve("passwd", "../../etc") is None
    assert resolver.resolve("../secret.mp4", "/CameraA") is None
    # Unique filename fallback
    assert resolver.resolve("unique.mp4", None) == file_a
    # Ambiguous duplicate rejected when folder unknown
    assert resolver.resolve("dup.mp4", None) is None
    # Ignored @eaDir
    assert resolver.resolve("SYNOPHOTO_THUMB_XL.jpg", None) is None


def test_dsm_client_login_2fa_list_fallback_and_upload(tmp_path: Path) -> None:
    captured_requests: list[tuple[str, str]] = []

    def handler(request: httpx.Request) -> httpx.Response:
        body_text = request.content.decode("utf-8", errors="ignore")
        captured_requests.append((request.headers.get("content-type", ""), body_text))

        if "method=login" in body_text:
            if "account=otpuser" in body_text and "otp_code=" not in body_text:
                return httpx.Response(200, json={"success": False, "error": {"code": 403}})
            return httpx.Response(
                200,
                json={
                    "success": True,
                    "data": {
                        "sid": "sid-abc",
                        "device_id": "did-xyz",
                        "synotoken": "tok-999",
                    },
                },
            )

        if "method=list_convert_needed" in body_text:
            if "video_need_thumbnail" in body_text:
                return httpx.Response(200, json={"success": False, "error": {"code": 120}})
            return httpx.Response(
                200,
                json={
                    "success": True,
                    "data": {
                        "list": [
                            {"unit_id": 10, "filename": "a.MP4", "type": 1, "need_thumbnail": True, "need_video": True},
                            {"unit_id": 11, "filename": "b.MP4", "type": 1, "need_thumbnail": False, "need_video": True},
                        ]
                    },
                },
            )

        if "SYNO.Foto.Upload.ConvertedFile" in body_text and "method" in body_text:
            assert "multipart/form-data" in request.headers.get("content-type", "")
            assert 'name="thumb_sm"' in body_text
            assert 'name="thumb_m"' in body_text
            assert 'name="thumb_xl"' in body_text
            return httpx.Response(200, json={"success": True})

        if "method=logout" in body_text:
            return httpx.Response(200, json={"success": True})

        raise AssertionError(f"Unexpected request: {body_text[:200]}")

    transport = httpx.MockTransport(handler)
    state = StateStore(tmp_path / "state.json")

    otp_settings = Settings(
        dsm_url="http://127.0.0.1:5000",
        dsm_username="otpuser",
        dsm_password="secret",
        data_dir=tmp_path,
    )
    with httpx.Client(transport=transport) as http:
        otp_client = DsmPhotosClient(otp_settings, state, http_client=http)
        with pytest.raises(DsmAuthFatalError) as exc_info:
            otp_client.login()
        assert exc_info.value.requires_otp is True

    settings = Settings(
        dsm_url="http://127.0.0.1:5000",
        dsm_username="testuser",
        dsm_password="secret",
        data_dir=tmp_path,
    )
    with httpx.Client(transport=transport) as http:
        client = DsmPhotosClient(settings, state, http_client=http)
        assert client.login() == "did-xyz"
        assert state.device_id == "did-xyz"

        items = client.list_convert_needed()
        assert [i["unit_id"] for i in items] == [10]
        assert client._fallback_video_type is True

        sm = tmp_path / "sm.jpg"
        m = tmp_path / "m.jpg"
        xl = tmp_path / "xl.jpg"
        for p in (sm, m, xl):
            p.write_bytes(b"\xff\xd8\xff\xd9")
        client.upload_thumbnails(10, sm, m, xl)
        client.logout()


def test_generate_image_thumbnails_with_synthetic_jpeg(tmp_path: Path) -> None:
    src = tmp_path / "sample.jpg"
    Image.new("RGB", (1600, 900), color=(40, 120, 200)).save(src, format="JPEG")
    out_dir = tmp_path / "out"

    sm, m, xl = generate_image_thumbnails(src, out_dir, jpeg_quality=85)
    with Image.open(xl) as img_xl, Image.open(m) as img_m, Image.open(sm) as img_sm:
        assert img_xl.size == (1600, 900)
        assert img_m.size == (569, 320)
        assert img_sm.size == (427, 240)


def test_cmd_auth_with_slotted_settings_and_otp(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        body_text = request.content.decode("utf-8", errors="ignore")
        if "method=login" in body_text:
            if "otp_code=654321" not in body_text:
                return httpx.Response(200, json={"success": False, "error": {"code": 403}})
            return httpx.Response(
                200,
                json={
                    "success": True,
                    "data": {
                        "sid": "sid-otp",
                        "did": "did-from-otp",
                        "synotoken": "tok-otp",
                    },
                },
            )
        if "method=list_convert_needed" in body_text:
            return httpx.Response(200, json={"success": True, "data": {"list": []}})
        if "method=logout" in body_text:
            return httpx.Response(200, json={"success": True})
        raise AssertionError(f"Unexpected request: {body_text}")

    transport = httpx.MockTransport(handler)
    real_client = httpx.Client
    monkeypatch.setattr(
        "synology_thumbgen.dsm_client.httpx.Client",
        lambda **kwargs: real_client(transport=transport),
    )
    monkeypatch.setattr("builtins.input", lambda _prompt="": "654321")

    settings = Settings(
        dsm_url="http://127.0.0.1:5000",
        dsm_username="testuser",
        dsm_password="secret",
        data_dir=tmp_path,
    )
    rc = cmd_auth(settings)
    assert rc == 0
    saved = json.loads((tmp_path / "state.json").read_text(encoding="utf-8"))
    assert saved["device_id"] == "did-from-otp"


def test_settings_watch_defaults() -> None:
    s_default = Settings.from_env({})
    assert s_default.watch_enabled is True
    assert s_default.poll_interval_seconds == 1800
    assert s_default.watch_debounce_seconds == 15
    assert s_default.watch_followup_seconds == 45

    s_no_watch = Settings.from_env({"WATCH_ENABLED": "false"})
    assert s_no_watch.watch_enabled is False
    assert s_no_watch.poll_interval_seconds == 300

    s_explicit = Settings.from_env({"WATCH_ENABLED": "true", "POLL_INTERVAL_SECONDS": "600"})
    assert s_explicit.poll_interval_seconds == 600

    s_explicit_no_watch = Settings.from_env({"WATCH_ENABLED": "false", "POLL_INTERVAL_SECONDS": "900"})
    assert s_explicit_no_watch.poll_interval_seconds == 900


def test_inotify_watcher_detects_media_ignores_eadir_and_lrf_and_watches_new_subdirs(
    tmp_path: Path,
) -> None:
    photos = tmp_path / "Photos"
    existing_dir = photos / "CameraA" / "202501"
    ea_dir = existing_dir / "@eaDir"
    existing_dir.mkdir(parents=True)
    ea_dir.mkdir(parents=True)

    events: list[tuple[Path | None, str]] = []
    event_lock = threading.Lock()
    got_event = threading.Event()

    def on_event(path: Path | None, reason: str) -> None:
        with event_lock:
            events.append((path, reason))
        got_event.set()

    watcher = MediaInotifyWatcher(photos, on_event)
    try:
        watched_count = watcher.start()
        assert watched_count == 3  # Photos, CameraA, 202501 (@eaDir excluded)

        # Ignored files: @eaDir, .LRF, dotfile
        (ea_dir / "SYNOPHOTO_THUMB_XL.jpg").write_bytes(b"thumb")
        (existing_dir / "CLIP_0001.LRF").write_bytes(b"lrf")
        (existing_dir / ".hidden.MP4").write_bytes(b"hidden")
        assert got_event.wait(timeout=0.3) is False
        assert events == []

        # Valid MP4 file triggers close_write
        mp4_path = existing_dir / "CLIP_0001.MP4"
        mp4_path.write_bytes(b"mp4data")
        assert got_event.wait(timeout=2.0) is True
        with event_lock:
            assert any(p == mp4_path and r == "close_write" for p, r in events)
            events.clear()
        got_event.clear()

        # Brand-new nested subdirectory + HEIC file triggers dynamic watch / scan
        new_subdir = photos / "CameraA" / "202502"
        new_subdir.mkdir()
        heic_path = new_subdir / "IMG_0001.HEIC"
        # Small pause or direct write: either new_dir_scan or close_write should fire
        heic_path.write_bytes(b"heicdata")
        assert got_event.wait(timeout=2.0) is True
        with event_lock:
            assert any(p == heic_path for p, _ in events)
    finally:
        watcher.stop()
