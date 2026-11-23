use crate::config::Settings;
use crate::log_info;
use crate::state::{now_unix_secs, StateStore};
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File};
use std::io;
use std::path::Path;
use std::time::Duration;
use ureq::tls::TlsConfig;
use ureq::Agent;

const FATAL_AUTH_CODES: &[i64] = &[400, 401, 402, 403, 404, 407];
const SESSION_EXPIRED_CODES: &[i64] = &[105, 106, 107, 119];
const PARAM_ERROR_CODES: &[i64] = &[101, 102, 120];

#[derive(Debug)]
pub enum DsmError {
    AuthFatal {
        message: String,
        requires_otp: bool,
    },
    SessionExpired(String),
    Api {
        message: String,
    },
    Http(String),
    Io(io::Error),
}

impl fmt::Display for DsmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DsmError::AuthFatal { message, .. } => write!(f, "{message}"),
            DsmError::SessionExpired(msg) => write!(f, "{msg}"),
            DsmError::Api { message, .. } => write!(f, "{message}"),
            DsmError::Http(msg) => write!(f, "{msg}"),
            DsmError::Io(err) => write!(f, "{err}"),
        }
    }
}

impl From<io::Error> for DsmError {
    fn from(err: io::Error) -> Self {
        DsmError::Io(err)
    }
}

impl From<ureq::Error> for DsmError {
    fn from(err: ureq::Error) -> Self {
        DsmError::Http(err.to_string())
    }
}

#[derive(Debug, Clone)]
pub struct ConvertItem {
    pub unit_id: i64,
    pub filename: String,
    pub item_type: i32,
    pub need_thumbnail: bool,
    pub need_video: bool,
}

pub struct DsmPhotosClient {
    settings: Settings,
    agent: Agent,
    pub sid: Option<String>,
    pub did: Option<String>,
    pub syno_token: Option<String>,
    fallback_video_type: bool,
    folder_cache: HashMap<i64, String>,
}

impl DsmPhotosClient {
    pub fn new(settings: Settings, initial_device_id: Option<String>) -> Self {
        let tls = TlsConfig::builder()
            .disable_verification(!settings.verify_ssl)
            .build();
        let config = Agent::config_builder()
            .tls_config(tls)
            .max_idle_connections(0)
            .timeout_global(Some(Duration::from_secs(60)))
            .build();
        let agent: Agent = config.into();

        Self {
            settings,
            agent,
            sid: None,
            did: initial_device_id,
            syno_token: None,
            fallback_video_type: false,
            folder_cache: HashMap::new(),
        }
    }

    fn entry_url(&self) -> String {
        format!("{}/webapi/entry.cgi", self.settings.dsm_url)
    }

    pub fn clear_folder_cache(&mut self) {
        self.folder_cache.clear();
    }

    fn cookie_header(&self) -> Option<String> {
        let mut parts = Vec::with_capacity(2);
        if let Some(did) = &self.did {
            if !did.is_empty() {
                parts.push(format!("did={did}"));
            }
        }
        if let Some(sid) = &self.sid {
            if !sid.is_empty() {
                parts.push(format!("id={sid}"));
            }
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("; "))
        }
    }

    fn apply_auth_headers<B>(&self, mut req: ureq::RequestBuilder<B>) -> ureq::RequestBuilder<B> {
        if let Some(tok) = &self.syno_token {
            if !tok.is_empty() {
                req = req.header("X-Syno-Token", tok.as_str());
            }
        }
        if let Some(cookie) = self.cookie_header() {
            req = req.header("Cookie", cookie);
        }
        req
    }

    fn check_api_response(payload: &Value, action: &str) -> Result<(), DsmError> {
        if payload.get("success").and_then(Value::as_bool) == Some(true) {
            return Ok(());
        }
        let err = payload.get("error").cloned().unwrap_or(Value::Null);
        let code = err.get("code").and_then(Value::as_i64);
        if let Some(c) = code {
            if SESSION_EXPIRED_CODES.contains(&c) {
                return Err(DsmError::SessionExpired(format!(
                    "{action} failed with session error {c}: {err}"
                )));
            }
        }
        Err(DsmError::Api {
            message: format!("{action} failed with error {err}"),
        })
    }

    pub fn login(
        &mut self,
        state: &mut StateStore,
        otp_code: Option<&str>,
    ) -> Result<Option<String>, DsmError> {
        let mut form: Vec<(&str, &str)> = vec![
            ("api", "SYNO.API.Auth"),
            ("version", "7"),
            ("method", "login"),
            ("enable_syno_token", "yes"),
            ("enable_device_token", "yes"),
            ("format", "sid"),
            ("device_name", &self.settings.device_name),
            ("account", &self.settings.dsm_username),
            ("passwd", &self.settings.dsm_password),
        ];
        if let Some(did) = state.device_id.as_deref() {
            if !did.is_empty() {
                form.push(("device_id", did));
            }
        }
        if let Some(otp) = otp_code {
            if !otp.is_empty() {
                form.push(("otp_code", otp));
            }
        }

        let url = self.entry_url();
        let mut resp = self.agent.post(&url).send_form(form)?;
        let payload: Value = resp.body_mut().read_json()?;

        if payload.get("success").and_then(Value::as_bool) != Some(true) {
            let err = payload.get("error").cloned().unwrap_or(Value::Null);
            let code = err.get("code").and_then(Value::as_i64);
            if let Some(c) = code {
                if FATAL_AUTH_CODES.contains(&c) {
                    return Err(DsmError::AuthFatal {
                        message: format!("DSM authentication failed (code={c}): {err}"),
                        requires_otp: c == 403,
                    });
                }
            }
            return Err(DsmError::Api {
                message: format!("DSM login failed: {err}"),
            });
        }

        let data = payload.get("data").unwrap_or(&Value::Null);
        self.sid = data
            .get("sid")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        self.did = data
            .get("device_id")
            .or_else(|| data.get("did"))
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .or_else(|| state.device_id.clone());
        self.syno_token = data
            .get("synotoken")
            .and_then(Value::as_str)
            .map(ToString::to_string);

        if let Some(did) = &self.did {
            if let Err(err) = state.set_device_id(Some(did.clone())) {
                return Err(DsmError::Io(io::Error::other(err)));
            }
        }
        Ok(self.did.clone())
    }

    pub fn logout(&mut self) {
        if self.sid.is_none() {
            return;
        }
        let url = self.entry_url();
        let req = self
            .apply_auth_headers(self.agent.post(&url))
            .config()
            .timeout_global(Some(Duration::from_secs(10)))
            .build();
        let _ = req.send_form([
            ("api", "SYNO.API.Auth"),
            ("version", "7"),
            ("method", "logout"),
        ]);
        self.sid = None;
        self.syno_token = None;
    }

    fn post_list_convert_needed(&self, type_arg: &str) -> Result<Value, DsmError> {
        let url = self.entry_url();
        let req = self.apply_auth_headers(self.agent.post(&url));
        let mut resp = req.send_form([
            ("api", "SYNO.Foto.Upload.ConvertedFile"),
            ("version", "3"),
            ("method", "list_convert_needed"),
            ("type", type_arg),
            ("preset", "windows"),
        ])?;
        Ok(resp.body_mut().read_json()?)
    }

    pub fn list_convert_needed(&mut self) -> Result<Vec<ConvertItem>, DsmError> {
        let primary_type = if self.fallback_video_type {
            r#"["photo","video","live_video"]"#
        } else {
            r#"["photo","video_need_thumbnail"]"#
        };
        let mut payload = self.post_list_convert_needed(primary_type)?;
        if payload.get("success").and_then(Value::as_bool) != Some(true) && !self.fallback_video_type
        {
            let code = payload
                .get("error")
                .and_then(|e| e.get("code"))
                .and_then(Value::as_i64);
            if let Some(c) = code {
                if PARAM_ERROR_CODES.contains(&c) {
                    log_info!(
                        "list_convert_needed rejected video_need_thumbnail (code={}); falling back to standard type filter",
                        c
                    );
                    self.fallback_video_type = true;
                    payload = self.post_list_convert_needed(r#"["photo","video","live_video"]"#)?;
                }
            }
        }
        Self::check_api_response(&payload, "list_convert_needed")?;

        let raw_list = payload
            .get("data")
            .and_then(|d| d.get("list"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let mut items = Vec::with_capacity(raw_list.len());
        for entry in raw_list {
            let need_thumbnail = entry
                .get("need_thumbnail")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            if !need_thumbnail {
                continue;
            }
            let Some(unit_id) = entry.get("unit_id").and_then(Value::as_i64) else {
                continue;
            };
            let filename = entry
                .get("filename")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let item_type = entry.get("type").and_then(Value::as_i64).unwrap_or(1) as i32;
            let need_video = entry
                .get("need_video")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            items.push(ConvertItem {
                unit_id,
                filename,
                item_type,
                need_thumbnail,
                need_video,
            });
        }
        Ok(items)
    }

    pub fn resolve_folder_path(&mut self, unit_id: i64) -> Result<Option<String>, DsmError> {
        let url = self.entry_url();
        let unit_id_json = format!("[{unit_id}]");
        let unit_req = self.apply_auth_headers(self.agent.post(&url));
        let unit_payload: Value = match unit_req.send_form([
            ("api", "SYNO.Foto.Browse.Unit"),
            ("version", "3"),
            ("method", "get"),
            ("id", unit_id_json.as_str()),
        ]) {
            Ok(mut r) => r.body_mut().read_json()?,
            Err(_) => return Ok(None),
        };
        if let Err(err) = Self::check_api_response(&unit_payload, &format!("browse unit {unit_id}"))
        {
            return match err {
                DsmError::SessionExpired(_) => Err(err),
                _ => Ok(None),
            };
        }

        let Some(unit_info) = unit_payload
            .get("data")
            .and_then(|d| d.get("list"))
            .and_then(Value::as_array)
            .and_then(|l| l.first())
        else {
            return Ok(None);
        };

        let mut folder_id = unit_info.get("folder_id").and_then(Value::as_i64);
        if folder_id.is_none() {
            if let Some(item_id) = unit_info.get("item_id").and_then(Value::as_i64) {
                let item_id_json = format!("[{item_id}]");
                let item_req = self.apply_auth_headers(self.agent.post(&url));
                let item_payload: Value = match item_req.send_form([
                    ("api", "SYNO.Foto.Browse.Item"),
                    ("version", "7"),
                    ("method", "get"),
                    ("id", item_id_json.as_str()),
                ]) {
                    Ok(mut r) => r.body_mut().read_json()?,
                    Err(_) => return Ok(None),
                };
                if let Err(err) =
                    Self::check_api_response(&item_payload, &format!("browse item {item_id}"))
                {
                    return match err {
                        DsmError::SessionExpired(_) => Err(err),
                        _ => Ok(None),
                    };
                }
                folder_id = item_payload
                    .get("data")
                    .and_then(|d| d.get("list"))
                    .and_then(Value::as_array)
                    .and_then(|l| l.first())
                    .and_then(|item| item.get("folder_id"))
                    .and_then(Value::as_i64);
            }
        }

        let Some(fid) = folder_id else {
            return Ok(None);
        };
        if let Some(cached) = self.folder_cache.get(&fid) {
            return Ok(Some(cached.clone()));
        }

        let fid_str = fid.to_string();
        let folder_req = self.apply_auth_headers(self.agent.post(&url));
        let folder_payload: Value = match folder_req.send_form([
            ("api", "SYNO.Foto.Browse.Folder"),
            ("version", "2"),
            ("method", "get"),
            ("id", fid_str.as_str()),
        ]) {
            Ok(mut r) => r.body_mut().read_json()?,
            Err(_) => return Ok(None),
        };
        if let Err(err) = Self::check_api_response(&folder_payload, &format!("browse folder {fid}"))
        {
            return match err {
                DsmError::SessionExpired(_) => Err(err),
                _ => Ok(None),
            };
        }

        if let Some(folder_name) = folder_payload
            .get("data")
            .and_then(|d| d.get("folder"))
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            let name = folder_name.to_string();
            self.folder_cache.insert(fid, name.clone());
            return Ok(Some(name));
        }
        Ok(None)
    }

    pub fn download_unit(&self, unit_id: i64, dest_path: &Path) -> Result<(), DsmError> {
        let url = self.entry_url();
        let unit_arg = format!("[{unit_id}]");
        let req = self
            .apply_auth_headers(self.agent.get(&url))
            .query("api", "SYNO.Foto.Download")
            .query("version", "1")
            .query("method", "download")
            .query("unit_id", &unit_arg)
            .config()
            .timeout_global(Some(Duration::from_secs(300)))
            .build();
        let mut resp = req.call()?;
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if content_type.contains("json") {
            let payload: Value = resp.body_mut().read_json()?;
            Self::check_api_response(&payload, &format!("download unit {unit_id}"))?;
            return Err(DsmError::Api {
                message: format!("Unexpected JSON response downloading unit {unit_id}"),
            });
        }
        let mut reader = resp.body_mut().as_reader();
        let mut file = File::create(dest_path)?;
        io::copy(&mut reader, &mut file)?;
        Ok(())
    }

    pub fn upload_thumbnails(
        &self,
        unit_id: i64,
        thumb_sm: &Path,
        thumb_m: &Path,
        thumb_xl: &Path,
    ) -> Result<(), DsmError> {
        let boundary = format!(
            "----SynologyThumbGen{:x}{:x}",
            now_unix_secs().to_bits(),
            std::process::id()
        );
        let mut body = Vec::with_capacity(256 * 1024);

        let unit_str = unit_id.to_string();
        for (name, value) in [
            ("api", "SYNO.Foto.Upload.ConvertedFile"),
            ("version", "3"),
            ("method", "upload"),
            ("unit_id", unit_str.as_str()),
        ] {
            append_multipart_text(&mut body, &boundary, name, value);
        }

        for (field, filename, path) in [
            ("thumb_sm", "thumb_sm.jpg", thumb_sm),
            ("thumb_m", "thumb_m.jpg", thumb_m),
            ("thumb_xl", "thumb_xl.jpg", thumb_xl),
        ] {
            let bytes = fs::read(path)?;
            append_multipart_file(&mut body, &boundary, field, filename, "image/jpeg", &bytes);
        }

        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

        let url = self.entry_url();
        let req = self
            .apply_auth_headers(self.agent.post(&url))
            .header("Content-Type", format!("multipart/form-data; boundary={boundary}"))
            .config()
            .timeout_global(Some(Duration::from_secs(120)))
            .build();

        let mut resp = req.send(body)?;
        let payload: Value = resp.body_mut().read_json()?;
        Self::check_api_response(&payload, &format!("upload thumbnails for unit {unit_id}"))
    }

    pub fn set_broken(&self, unit_id: i64) -> Result<(), DsmError> {
        let url = self.entry_url();
        let id_arg = format!("[{unit_id}]");
        let req = self.apply_auth_headers(self.agent.post(&url));
        let mut resp = req.send_form([
            ("api", "SYNO.Foto.Upload.ConvertedFile"),
            ("version", "3"),
            ("method", "set_broken"),
            ("id", id_arg.as_str()),
            ("type", r#"["photo"]"#),
        ])?;
        let payload: Value = resp.body_mut().read_json()?;
        Self::check_api_response(&payload, &format!("set_broken unit {unit_id}"))
    }
}

fn append_multipart_text(buf: &mut Vec<u8>, boundary: &str, name: &str, value: &str) {
    buf.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
        )
        .as_bytes(),
    );
}

fn append_multipart_file(
    buf: &mut Vec<u8>,
    boundary: &str,
    field: &str,
    filename: &str,
    content_type: &str,
    bytes: &[u8],
) {
    buf.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
        )
        .as_bytes(),
    );
    buf.extend_from_slice(bytes);
    buf.extend_from_slice(b"\r\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::thread;

    fn read_http_request(stream: &mut std::net::TcpStream) -> (String, HashMap<String, String>, Vec<u8>) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();

        let mut headers = HashMap::new();
        let mut content_len = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            if let Some((k, v)) = trimmed.split_once(':') {
                let key = k.trim().to_ascii_lowercase();
                let val = v.trim().to_string();
                if key == "content-length" {
                    content_len = val.parse().unwrap_or(0);
                }
                headers.insert(key, val);
            }
        }
        let mut body = vec![0u8; content_len];
        if content_len > 0 {
            io::Read::read_exact(&mut reader, &mut body).unwrap();
        }
        (request_line, headers, body)
    }

    fn write_json_response(stream: &mut std::net::TcpStream, body: &str) {
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(resp.as_bytes()).unwrap();
        stream.flush().unwrap();
    }

    #[test]
    fn test_dsm_client_login_2fa_list_fallback_and_upload() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let server = thread::spawn(move || {
            for _ in 0..6 {
                let (mut stream, _) = listener.accept().unwrap();
                let (_req_line, headers, body) = read_http_request(&mut stream);
                let body_text = String::from_utf8_lossy(&body);

                if body_text.contains("method=login") {
                    if body_text.contains("account=otpuser") && !body_text.contains("otp_code=") {
                        write_json_response(&mut stream, r#"{"success":false,"error":{"code":403}}"#);
                    } else {
                        write_json_response(
                            &mut stream,
                            r#"{"success":true,"data":{"sid":"sid-abc","device_id":"did-xyz","synotoken":"tok-999"}}"#,
                        );
                    }
                } else if body_text.contains("method=list_convert_needed") {
                    if body_text.contains("video_need_thumbnail") {
                        write_json_response(&mut stream, r#"{"success":false,"error":{"code":120}}"#);
                    } else {
                        write_json_response(
                            &mut stream,
                            r#"{"success":true,"data":{"list":[{"unit_id":10,"filename":"a.MP4","type":1,"need_thumbnail":true,"need_video":true},{"unit_id":11,"filename":"b.MP4","type":1,"need_thumbnail":false,"need_video":true}]}}"#,
                        );
                    }
                } else if body_text.contains("SYNO.Foto.Upload.ConvertedFile") {
                    let ct = headers.get("content-type").cloned().unwrap_or_default();
                    assert!(ct.contains("multipart/form-data"));
                    assert!(body_text.contains(r#"name="thumb_sm""#));
                    assert!(body_text.contains(r#"name="thumb_m""#));
                    assert!(body_text.contains(r#"name="thumb_xl""#));
                    write_json_response(&mut stream, r#"{"success":true}"#);
                } else if body_text.contains("method=logout") {
                    write_json_response(&mut stream, r#"{"success":true}"#);
                } else {
                    panic!("Unexpected request: {body_text}");
                }
            }
        });

        let dir = std::env::temp_dir().join(format!(
            "thumbgen_dsm_test_{}_{}",
            std::process::id(),
            now_unix_secs().to_bits()
        ));
        fs::create_dir_all(&dir).unwrap();
        let mut state = StateStore::load(dir.join("state.json")).unwrap();

        let otp_settings = Settings {
            dsm_url: format!("http://{addr}"),
            dsm_username: "otpuser".into(),
            dsm_password: "secret".into(),
            data_dir: dir.clone(),
            ..Settings::default()
        };
        let mut otp_client = DsmPhotosClient::new(otp_settings, None);
        let err = otp_client.login(&mut state, None).unwrap_err();
        assert!(matches!(err, DsmError::AuthFatal { requires_otp: true, .. }));

        let settings = Settings {
            dsm_url: format!("http://{addr}"),
            dsm_username: "testuser".into(),
            dsm_password: "secret".into(),
            data_dir: dir.clone(),
            ..Settings::default()
        };
        let mut client = DsmPhotosClient::new(settings, state.device_id.clone());
        assert_eq!(client.login(&mut state, None).unwrap().as_deref(), Some("did-xyz"));
        assert_eq!(state.device_id.as_deref(), Some("did-xyz"));

        let items = client.list_convert_needed().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].unit_id, 10);
        assert!(client.fallback_video_type);

        let sm = dir.join("sm.jpg");
        let m = dir.join("m.jpg");
        let xl = dir.join("xl.jpg");
        for p in [&sm, &m, &xl] {
            fs::write(p, b"\xff\xd8\xff\xd9").unwrap();
        }
        client.upload_thumbnails(10, &sm, &m, &xl).unwrap();
        client.logout();

        server.join().unwrap();
        let _ = fs::remove_dir_all(dir);
    }
}
