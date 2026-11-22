from __future__ import annotations

import logging
from pathlib import Path
from typing import Any

import httpx

from .config import Settings
from .state import StateStore

logger = logging.getLogger(__name__)

FATAL_AUTH_ERROR_CODES = frozenset({400, 401, 402, 403, 404, 407})
SESSION_EXPIRED_ERROR_CODES = frozenset({105, 106, 107, 119})
PARAM_ERROR_CODES = frozenset({101, 102, 120})


class DsmAuthFatalError(Exception):
    """Non-retryable authentication failure (prevents DSM Auto Block loops)."""

    def __init__(self, message: str, code: int | None = None, requires_otp: bool = False) -> None:
        super().__init__(message)
        self.code = code
        self.requires_otp = requires_otp


class DsmSessionExpiredError(Exception):
    """Session expired or invalid; caller may re-authenticate once."""


class DsmApiError(Exception):
    """General Synology WebAPI failure."""

    def __init__(self, message: str, code: int | None = None) -> None:
        super().__init__(message)
        self.code = code


class DsmPhotosClient:
    """Thin client for Synology DSM Auth and Synology Photos ConvertedFile APIs."""

    def __init__(
        self,
        settings: Settings,
        state: StateStore,
        http_client: httpx.Client | None = None,
    ) -> None:
        self.settings = settings
        self.state = state
        self._owns_client = http_client is None
        self.http = http_client or httpx.Client(
            verify=settings.verify_ssl,
            timeout=60.0,
            limits=httpx.Limits(keepalive_expiry=30.0),
        )
        self.sid: str | None = None
        self.did: str | None = state.device_id
        self.syno_token: str | None = None
        self._fallback_video_type = False
        self.folder_cache: dict[int, str] = {}

    @property
    def entry_url(self) -> str:
        return f"{self.settings.dsm_url}/webapi/entry.cgi"

    def __enter__(self) -> DsmPhotosClient:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()

    def close(self) -> None:
        if self._owns_client:
            self.http.close()

    def _auth_headers(self) -> dict[str, str]:
        headers: dict[str, str] = {}
        if self.syno_token:
            headers["X-Syno-Token"] = self.syno_token
        cookies: list[str] = []
        if self.did:
            cookies.append(f"did={self.did}")
        if self.sid:
            cookies.append(f"id={self.sid}")
        if cookies:
            headers["Cookie"] = "; ".join(cookies)
        return headers

    def _check_api_response(self, payload: dict[str, Any], action: str) -> None:
        if payload.get("success"):
            return
        err = payload.get("error") or {}
        code = err.get("code")
        if code in SESSION_EXPIRED_ERROR_CODES:
            raise DsmSessionExpiredError(f"{action} failed with session error {code}: {err}")
        raise DsmApiError(f"{action} failed with error {err}", code=code)

    def login(self, otp_code: str | None = None) -> str | None:
        form: dict[str, str] = {
            "api": "SYNO.API.Auth",
            "version": "7",
            "method": "login",
            "enable_syno_token": "yes",
            "enable_device_token": "yes",
            "format": "sid",
            "device_name": self.settings.device_name,
            "account": self.settings.dsm_username,
            "passwd": self.settings.dsm_password,
        }
        if self.state.device_id:
            form["device_id"] = self.state.device_id
        if otp_code:
            form["otp_code"] = otp_code

        resp = self.http.post(self.entry_url, data=form)
        resp.raise_for_status()
        payload = resp.json()

        if not payload.get("success"):
            err = payload.get("error") or {}
            code = err.get("code")
            if code in FATAL_AUTH_ERROR_CODES:
                raise DsmAuthFatalError(
                    f"DSM authentication failed (code={code}): {err}",
                    code=code,
                    requires_otp=(code == 403),
                )
            raise DsmApiError(f"DSM login failed: {err}", code=code)

        data = payload.get("data") or {}
        self.sid = data.get("sid")
        self.did = data.get("device_id") or data.get("did") or self.state.device_id
        self.syno_token = data.get("synotoken")
        if self.did:
            self.state.set_device_id(self.did)
        return self.did

    def logout(self) -> None:
        if not self.sid:
            return
        try:
            self.http.post(
                self.entry_url,
                headers=self._auth_headers(),
                data={
                    "api": "SYNO.API.Auth",
                    "version": "7",
                    "method": "logout",
                    "session": "SynologyPhotosThumbGen",
                },
                timeout=10.0,
            )
        except Exception as exc:
            logger.debug("Ignoring logout error: %s", exc)
        finally:
            self.sid = None
            self.syno_token = None

    def list_convert_needed(self) -> list[dict[str, Any]]:
        type_arg = (
            '["photo","video","live_video"]'
            if self._fallback_video_type
            else '["photo","video_need_thumbnail"]'
        )
        payload = self._post_list_convert_needed(type_arg)
        if not payload.get("success"):
            err = payload.get("error") or {}
            code = err.get("code")
            if not self._fallback_video_type and code in PARAM_ERROR_CODES:
                logger.info(
                    "list_convert_needed rejected video_need_thumbnail (code=%s); "
                    "falling back to standard type filter",
                    code,
                )
                self._fallback_video_type = True
                payload = self._post_list_convert_needed('["photo","video","live_video"]')
        self._check_api_response(payload, "list_convert_needed")
        items = (payload.get("data") or {}).get("list") or []
        return [item for item in items if item.get("need_thumbnail", True)]

    def _post_list_convert_needed(self, type_arg: str) -> dict[str, Any]:
        resp = self.http.post(
            self.entry_url,
            headers=self._auth_headers(),
            data={
                "api": "SYNO.Foto.Upload.ConvertedFile",
                "version": "3",
                "method": "list_convert_needed",
                "type": type_arg,
                "preset": "windows",
            },
        )
        resp.raise_for_status()
        return resp.json()

    def resolve_folder_path(self, unit_id: int) -> str | None:
        try:
            unit_resp = self.http.post(
                self.entry_url,
                headers=self._auth_headers(),
                data={
                    "api": "SYNO.Foto.Browse.Unit",
                    "version": "3",
                    "method": "get",
                    "id": f"[{unit_id}]",
                },
            )
            unit_resp.raise_for_status()
            unit_payload = unit_resp.json()
            self._check_api_response(unit_payload, f"browse unit {unit_id}")
            units = (unit_payload.get("data") or {}).get("list") or []
            if not units:
                return None
            unit_info = units[0]
            folder_id = unit_info.get("folder_id")

            if folder_id is None and unit_info.get("item_id") is not None:
                item_resp = self.http.post(
                    self.entry_url,
                    headers=self._auth_headers(),
                    data={
                        "api": "SYNO.Foto.Browse.Item",
                        "version": "7",
                        "method": "get",
                        "id": f"[{unit_info['item_id']}]",
                    },
                )
                item_resp.raise_for_status()
                item_payload = item_resp.json()
                self._check_api_response(item_payload, f"browse item {unit_info['item_id']}")
                items = (item_payload.get("data") or {}).get("list") or []
                if items:
                    folder_id = items[0].get("folder_id")

            if folder_id is None:
                return None
            folder_id_int = int(folder_id)
            if folder_id_int in self.folder_cache:
                return self.folder_cache[folder_id_int]

            folder_resp = self.http.post(
                self.entry_url,
                headers=self._auth_headers(),
                data={
                    "api": "SYNO.Foto.Browse.Folder",
                    "version": "2",
                    "method": "get",
                    "id": str(folder_id_int),
                },
            )
            folder_resp.raise_for_status()
            folder_payload = folder_resp.json()
            self._check_api_response(folder_payload, f"browse folder {folder_id_int}")
            folder_name = ((folder_payload.get("data") or {}).get("folder") or {}).get("name")
            if isinstance(folder_name, str) and folder_name:
                self.folder_cache[folder_id_int] = folder_name
                return folder_name
            return None
        except DsmSessionExpiredError:
            raise
        except (httpx.HTTPError, DsmApiError, KeyError, TypeError, ValueError) as exc:
            logger.debug("Could not resolve folder path for unit_id=%s: %s", unit_id, exc)
            return None

    def download_unit(self, unit_id: int, dest_path: Path) -> None:
        params = {
            "api": "SYNO.Foto.Download",
            "version": "1",
            "method": "download",
            "unit_id": f"[{unit_id}]",
        }
        with self.http.stream(
            "GET",
            self.entry_url,
            params=params,
            headers=self._auth_headers(),
            timeout=300.0,
        ) as resp:
            content_type = (resp.headers.get("content-type") or "").lower()
            if "json" in content_type:
                resp.read()
                self._check_api_response(resp.json(), f"download unit {unit_id}")
                raise DsmApiError(f"Unexpected JSON response when downloading unit {unit_id}")
            resp.raise_for_status()
            with dest_path.open("wb") as fh:
                for chunk in resp.iter_bytes(chunk_size=1024 * 1024):
                    fh.write(chunk)

    def upload_thumbnails(
        self,
        unit_id: int,
        thumb_sm: Path,
        thumb_m: Path,
        thumb_xl: Path,
    ) -> None:
        with (
            thumb_sm.open("rb") as f_sm,
            thumb_m.open("rb") as f_m,
            thumb_xl.open("rb") as f_xl,
        ):
            resp = self.http.post(
                self.entry_url,
                headers=self._auth_headers(),
                data={
                    "api": "SYNO.Foto.Upload.ConvertedFile",
                    "version": "3",
                    "method": "upload",
                    "unit_id": str(unit_id),
                },
                files={
                    "thumb_sm": ("thumb_sm.jpg", f_sm, "image/jpeg"),
                    "thumb_m": ("thumb_m.jpg", f_m, "image/jpeg"),
                    "thumb_xl": ("thumb_xl.jpg", f_xl, "image/jpeg"),
                },
                timeout=120.0,
            )
        resp.raise_for_status()
        self._check_api_response(resp.json(), f"upload thumbnails for unit {unit_id}")

    def set_broken(self, unit_id: int, item_type: int = 0) -> None:
        resp = self.http.post(
            self.entry_url,
            headers=self._auth_headers(),
            data={
                "api": "SYNO.Foto.Upload.ConvertedFile",
                "version": "3",
                "method": "set_broken",
                "id": f"[{unit_id}]",
                "type": '["photo"]',
            },
        )
        resp.raise_for_status()
        self._check_api_response(resp.json(), f"set_broken unit {unit_id} (type={item_type})")
