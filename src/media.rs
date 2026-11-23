use crate::config::Settings;
use crate::log_debug;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

const SKIP_DIRS: &[&str] = &["@eaDir", "#recycle", "#snapshot"];
const IMAGE_EXTENSIONS: &[&str] = &["heic", "heif", "jpg", "jpeg", "png", "webp", "tif", "tiff", "bmp", "gif"];
const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mov", "m4v", "mkv", "avi", "mts", "m2ts", "wmv", "3gp", "webm"];
const HEIF_BRANDS: &[[u8; 4]] = &[
    *b"mif1", *b"msf1", *b"heic", *b"heix", *b"hevc", *b"hevx", *b"heim", *b"heis", *b"hevm",
    *b"hevs",
];

pub fn is_heif_container(path: &Path) -> bool {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut buf = [0u8; 12];
    let mut read = 0usize;
    while read < buf.len() {
        match file.read(&mut buf[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return false,
        }
    }
    read >= 12
        && &buf[4..8] == b"ftyp"
        && HEIF_BRANDS.iter().any(|brand| brand.as_slice() == &buf[8..12])
}

#[derive(Debug, Clone)]
pub struct VideoMeta {
    pub color_transfer: String,
    pub duration: f64,
}

#[derive(Debug, Deserialize)]
struct FfprobeOutput {
    #[serde(default)]
    streams: Vec<FfprobeStream>,
    format: Option<FfprobeFormat>,
}

#[derive(Debug, Deserialize)]
struct FfprobeStream {
    color_transfer: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FfprobeFormat {
    duration: Option<String>,
}

pub fn read_jpeg_dimensions(path: &Path) -> Result<(u32, u32), String> {
    let mut file = File::open(path)
        .map_err(|e| format!("Failed opening JPEG {}: {e}", path.display()))?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)
        .map_err(|e| format!("Failed reading JPEG {}: {e}", path.display()))?;

    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return Err(format!("Invalid JPEG header in {}", path.display()));
    }

    let mut i = 2usize;
    while i < data.len() {
        while i < data.len() && data[i] != 0xFF {
            i += 1;
        }
        while i < data.len() && data[i] == 0xFF {
            i += 1;
        }
        if i >= data.len() {
            break;
        }
        let marker = data[i];
        i += 1;

        if marker == 0xD8 || marker == 0xD9 || marker == 0x00 || (0xD0..=0xD7).contains(&marker) {
            continue;
        }
        if i + 2 > data.len() {
            break;
        }
        let seg_len = u16::from_be_bytes([data[i], data[i + 1]]) as usize;
        if seg_len < 2 || i + seg_len > data.len() {
            break;
        }
        if matches!(marker, 0xC0 | 0xC1 | 0xC2 | 0xC3) {
            if seg_len < 7 {
                break;
            }
            let height = u16::from_be_bytes([data[i + 3], data[i + 4]]) as u32;
            let width = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u32;
            if width > 0 && height > 0 {
                return Ok((width, height));
            }
            return Err(format!("Zero JPEG dimensions in {}", path.display()));
        }
        i += seg_len;
    }
    Err(format!("No SOF frame header found in JPEG {}", path.display()))
}

pub struct LocalMediaResolver {
    photos_dir: PathBuf,
    filename_index: Option<HashMap<String, Vec<PathBuf>>>,
}

impl LocalMediaResolver {
    pub fn new(photos_dir: PathBuf) -> Self {
        Self {
            photos_dir,
            filename_index: None,
        }
    }

    pub fn clear_index(&mut self) {
        self.filename_index = None;
    }

    fn build_index(&self) -> HashMap<String, Vec<PathBuf>> {
        let mut index: HashMap<String, Vec<PathBuf>> = HashMap::new();
        if !self.photos_dir.exists() {
            return index;
        }
        let mut stack = vec![self.photos_dir.clone()];
        while let Some(dir) = stack.pop() {
            let entries = match fs::read_dir(&dir) {
                Ok(rd) => rd,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let file_type = match entry.file_type() {
                    Ok(ft) => ft,
                    Err(_) => continue,
                };
                let name = entry.file_name().to_string_lossy().into_owned();
                if file_type.is_dir() {
                    if !SKIP_DIRS.contains(&name.as_str()) {
                        stack.push(entry.path());
                    }
                } else if file_type.is_file() {
                    index.entry(name).or_default().push(entry.path());
                }
            }
        }
        index
    }

    pub fn resolve(&mut self, filename: &str, folder_rel_path: Option<&str>) -> Option<PathBuf> {
        if filename.is_empty()
            || filename.contains('/')
            || filename.contains('\\')
            || filename == "."
            || filename == ".."
        {
            return None;
        }

        let root_canonical = fs::canonicalize(&self.photos_dir).ok()?;

        if let Some(rel_path) = folder_rel_path {
            let mut candidate = root_canonical.clone();
            let mut safe = true;
            for comp in Path::new(rel_path.trim_start_matches('/')).components() {
                match comp {
                    Component::Normal(part) => candidate.push(part),
                    Component::CurDir => {}
                    _ => {
                        safe = false;
                        break;
                    }
                }
            }
            if safe {
                candidate.push(filename);
                if let Ok(resolved) = fs::canonicalize(&candidate) {
                    if resolved.starts_with(&root_canonical) && resolved.is_file() {
                        return Some(resolved);
                    }
                }
            }
        }

        if self.filename_index.is_none() {
            self.filename_index = Some(self.build_index());
        }

        let found_in_cache = self
            .filename_index
            .as_ref()
            .and_then(|idx| idx.get(filename))
            .map(|v| !v.is_empty())
            .unwrap_or(false);

        if !found_in_cache {
            self.filename_index = Some(self.build_index());
        }

        let matches = self.filename_index.as_ref()?.get(filename)?;
        if matches.len() == 1 && matches[0].is_file() {
            Some(matches[0].clone())
        } else {
            None
        }
    }
}

pub fn probe_video(src_path: &Path, ffprobe_bin: &str) -> Result<VideoMeta, String> {
    let output = Command::new(ffprobe_bin)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height,codec_name,pix_fmt,color_transfer:format=duration",
            "-of",
            "json",
        ])
        .arg(src_path)
        .output()
        .map_err(|e| format!("Failed executing {ffprobe_bin}: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return Err(format!(
            "ffprobe failed for {}: {}",
            src_path.display(),
            if !stderr.is_empty() { stderr } else { stdout }
        ));
    }

    let parsed: FfprobeOutput = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("Failed parsing ffprobe output for {}: {e}", src_path.display()))?;
    let stream = parsed
        .streams
        .into_iter()
        .next()
        .ok_or_else(|| format!("No video stream found in {}", src_path.display()))?;

    let duration = parsed
        .format
        .and_then(|f| f.duration)
        .and_then(|d| d.parse::<f64>().ok())
        .unwrap_or(0.0);

    Ok(VideoMeta {
        color_transfer: stream.color_transfer.unwrap_or_default(),
        duration,
    })
}

fn build_thumbnail_filter_complex(use_hdr_tonemap: bool) -> String {
    let pre_filter = if use_hdr_tonemap {
        "zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv,format=yuvj420p"
    } else {
        "format=yuvj420p"
    };

    format!(
        "[0:v]{pre_filter},split=3[s_xl][s_m][s_sm];\
         [s_xl]scale=-2:'min(1280\\,ih)'[xl];\
         [s_m]scale='if(gt(iw\\,ih)\\,-2\\,min(320\\,iw))':'if(gt(iw\\,ih)\\,min(320\\,ih)\\,-2)'[m];\
         [s_sm]scale='if(gt(iw\\,ih)\\,-2\\,min(240\\,iw))':'if(gt(iw\\,ih)\\,min(240\\,ih)\\,-2)'[sm]"
    )
}

fn run_ffmpeg_thumbs(
    src_path: &Path,
    out_paths: (&Path, &Path, &Path),
    ffmpeg_bin: &str,
    seek_seconds: Option<f64>,
    use_hdr_tonemap: bool,
) -> Result<(), String> {
    let (thumb_xl, thumb_m, thumb_sm) = out_paths;
    let filter_complex = build_thumbnail_filter_complex(use_hdr_tonemap);

    let mut cmd = Command::new(ffmpeg_bin);
    cmd.args(["-hide_banner", "-v", "error", "-y"]);
    if let Some(seek) = seek_seconds {
        if seek > 0.0 {
            cmd.args(["-noaccurate_seek", "-ss", &format!("{seek:.3}")]);
        }
    }
    cmd.arg("-i")
        .arg(src_path)
        .args(["-filter_complex", &filter_complex]);

    for (label, target) in [("[xl]", thumb_xl), ("[m]", thumb_m), ("[sm]", thumb_sm)] {
        cmd.args([
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
        ])
        .arg(target);
    }

    let output = cmd
        .output()
        .map_err(|e| format!("Failed executing {ffmpeg_bin}: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let msg = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            format!("exit status {}", output.status)
        };
        return Err(msg);
    }

    for path in [thumb_xl, thumb_m, thumb_sm] {
        read_jpeg_dimensions(path)?;
    }
    Ok(())
}

pub fn generate_video_thumbnails(
    src_path: &Path,
    out_dir: &Path,
    ffmpeg_bin: &str,
    ffprobe_bin: &str,
) -> Result<(PathBuf, PathBuf, PathBuf), String> {
    fs::create_dir_all(out_dir)
        .map_err(|e| format!("Failed creating {}: {e}", out_dir.display()))?;

    let thumb_sm = out_dir.join("thumb_sm.jpg");
    let thumb_m = out_dir.join("thumb_m.jpg");
    let thumb_xl = out_dir.join("thumb_xl.jpg");

    let meta = probe_video(src_path, ffprobe_bin)?;
    let is_hdr = matches!(meta.color_transfer.as_str(), "arib-std-b67" | "smpte2084");
    let initial_seek = if meta.duration >= 1.5 { Some(1.0) } else { None };

    let mut attempts = vec![(initial_seek, is_hdr)];
    if is_hdr {
        attempts.push((initial_seek, false));
    }
    if initial_seek.is_some() {
        attempts.push((None, false));
    }

    let mut last_error = String::new();
    for (seek, hdr) in attempts {
        for p in [&thumb_xl, &thumb_m, &thumb_sm] {
            let _ = fs::remove_file(p);
        }
        match run_ffmpeg_thumbs(
            src_path,
            (&thumb_xl, &thumb_m, &thumb_sm),
            ffmpeg_bin,
            seek,
            hdr,
        ) {
            Ok(()) => return Ok((thumb_sm, thumb_m, thumb_xl)),
            Err(err) => {
                log_debug!(
                    "ffmpeg attempt failed (seek={:?} hdr={}) for {}: {}",
                    seek,
                    hdr,
                    src_path.display(),
                    err
                );
                last_error = err;
            }
        }
    }

    Err(format!(
        "ffmpeg failed to extract thumbnails for {}: {last_error}",
        src_path.display()
    ))
}

fn find_heif_decoded_png(work_dir: &Path) -> Option<PathBuf> {
    let direct = work_dir.join("decoded.png");
    if direct.is_file() {
        return Some(direct);
    }
    let mut candidates: Vec<PathBuf> = fs::read_dir(work_dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("decoded") && n.ends_with(".png"))
                    .unwrap_or(false)
        })
        .collect();
    candidates.sort();
    candidates.into_iter().next()
}

pub fn generate_image_thumbnails(
    src_path: &Path,
    out_dir: &Path,
    ffmpeg_bin: &str,
    heif_convert_bin: &str,
) -> Result<(PathBuf, PathBuf, PathBuf), String> {
    fs::create_dir_all(out_dir)
        .map_err(|e| format!("Failed creating {}: {e}", out_dir.display()))?;

    let thumb_sm = out_dir.join("thumb_sm.jpg");
    let thumb_m = out_dir.join("thumb_m.jpg");
    let thumb_xl = out_dir.join("thumb_xl.jpg");

    let mut temp_heif_dir: Option<PathBuf> = None;
    let input_for_ffmpeg = if is_heif_container(src_path) {
        let decode_dir = out_dir.join(".heif_tmp");
        fs::create_dir_all(&decode_dir)
            .map_err(|e| format!("Failed creating {}: {e}", decode_dir.display()))?;
        let decoded_target = decode_dir.join("decoded.png");
        let output = Command::new(heif_convert_bin)
            .arg(src_path)
            .arg(&decoded_target)
            .output()
            .map_err(|e| format!("Failed executing {heif_convert_bin}: {e}"))?;
        if !output.status.success() {
            let _ = fs::remove_dir_all(&decode_dir);
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            return Err(format!(
                "heif-convert failed for {}: {}",
                src_path.display(),
                if !stderr.is_empty() { stderr } else { stdout }
            ));
        }
        let found = find_heif_decoded_png(&decode_dir);
        temp_heif_dir = Some(decode_dir);
        match found {
            Some(path) => path,
            None => {
                if let Some(d) = temp_heif_dir {
                    let _ = fs::remove_dir_all(d);
                }
                return Err(format!(
                    "heif-convert produced no PNG output for {}",
                    src_path.display()
                ));
            }
        }
    } else {
        src_path.to_path_buf()
    };

    let res = run_ffmpeg_thumbs(
        &input_for_ffmpeg,
        (&thumb_xl, &thumb_m, &thumb_sm),
        ffmpeg_bin,
        None,
        false,
    );

    if let Some(d) = temp_heif_dir {
        let _ = fs::remove_dir_all(d);
    }

    res.map(|()| (thumb_sm, thumb_m, thumb_xl))
        .map_err(|e| format!("ffmpeg failed for image {}: {e}", src_path.display()))
}

pub fn generate_thumbnails(
    src_path: &Path,
    unit_type: Option<i32>,
    out_dir: &Path,
    settings: &Settings,
) -> Result<(PathBuf, PathBuf, PathBuf), String> {
    let ext = src_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();

    if unit_type == Some(0) || (unit_type.is_none() && IMAGE_EXTENSIONS.contains(&ext.as_str())) {
        return generate_image_thumbnails(
            src_path,
            out_dir,
            &settings.ffmpeg_bin,
            &settings.heif_convert_bin,
        );
    }
    if matches!(unit_type, Some(1 | 2))
        || (unit_type.is_none() && VIDEO_EXTENSIONS.contains(&ext.as_str()))
    {
        return generate_video_thumbnails(
            src_path,
            out_dir,
            &settings.ffmpeg_bin,
            &settings.ffprobe_bin,
        );
    }
    Err(format!(
        "Unsupported media file type (unit_type={unit_type:?}, ext={ext}): {}",
        src_path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("thumbgen_media_{}_{}_{}", label, std::process::id(), nanos));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_read_jpeg_dimensions_valid_and_invalid() {
        let dir = make_temp_dir("jpeg");
        let valid_path = dir.join("ok.jpg");
        // Minimal SOI + SOF0 (width=2276, height=1280) + EOI
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08];
        bytes.extend_from_slice(&1280u16.to_be_bytes());
        bytes.extend_from_slice(&2276u16.to_be_bytes());
        bytes.extend_from_slice(&[0x03, 0x01, 0x22, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01, 0xFF, 0xD9]);
        fs::write(&valid_path, &bytes).unwrap();
        assert_eq!(read_jpeg_dimensions(&valid_path).unwrap(), (2276, 1280));

        let bad_path = dir.join("bad.jpg");
        fs::write(&bad_path, b"not a jpeg").unwrap();
        assert!(read_jpeg_dimensions(&bad_path).is_err());

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_local_media_resolver_exact_traversal_unique_duplicate_and_clear() {
        let root = make_temp_dir("resolver");
        let photos = root.join("Photos");
        let dir_a = photos.join("CameraA").join("202501");
        let dir_b = photos.join("CameraB").join("202501");
        let ea_dir = dir_a.join("@eaDir").join("clip.mp4");
        fs::create_dir_all(&ea_dir).unwrap();
        fs::create_dir_all(&dir_b).unwrap();

        let file_a = dir_a.join("unique.mp4");
        fs::write(&file_a, b"a").unwrap();
        fs::write(ea_dir.join("SYNOPHOTO_THUMB_XL.jpg"), b"ignore").unwrap();
        fs::write(dir_a.join("dup.mp4"), b"d1").unwrap();
        let dup_b = dir_b.join("dup.mp4");
        fs::write(&dup_b, b"d2").unwrap();

        let mut resolver = LocalMediaResolver::new(photos);
        assert_eq!(
            resolver.resolve("dup.mp4", Some("/CameraB/202501")),
            Some(fs::canonicalize(&dup_b).unwrap())
        );
        assert_eq!(resolver.resolve("passwd", Some("../../etc")), None);
        assert_eq!(resolver.resolve("../secret.mp4", Some("/CameraA")), None);
        assert_eq!(
            resolver.resolve("unique.mp4", None),
            Some(fs::canonicalize(&file_a).unwrap())
        );
        assert_eq!(resolver.resolve("dup.mp4", None), None);
        assert_eq!(resolver.resolve("SYNOPHOTO_THUMB_XL.jpg", None), None);

        assert!(resolver.filename_index.is_some());
        resolver.clear_index();
        assert!(resolver.filename_index.is_none());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn test_is_heif_container_magic_bytes() {
        let dir = make_temp_dir("heif_magic");
        let heic = dir.join("real.heic");
        fs::write(&heic, b"\x00\x00\x00\x18ftypheic\x00\x00\x00\x00").unwrap();
        assert!(is_heif_container(&heic));

        let mif1 = dir.join("mif1.heif");
        fs::write(&mif1, b"\x00\x00\x00\x18ftypmif1\x00\x00\x00\x00").unwrap();
        assert!(is_heif_container(&mif1));

        let jpeg_as_heic = dir.join("fake.HEIC");
        fs::write(&jpeg_as_heic, b"\xff\xd8\xff\xe1\x00\x10Exif\x00\x00").unwrap();
        assert!(!is_heif_container(&jpeg_as_heic));

        let mp4 = dir.join("video.mp4");
        fs::write(&mp4, b"\x00\x00\x00\x1cftypisom\x00\x00\x02\x00").unwrap();
        assert!(!is_heif_container(&mp4));

        let _ = fs::remove_dir_all(dir);
    }
}
