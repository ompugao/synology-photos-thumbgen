use crate::log_warn;
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

const WATCH_MASK: u32 =
    libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO | libc::IN_CREATE | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF;
const SKIP_DIRS: &[&str] = &["@eaDir", "#recycle", "#snapshot"];
const INOTIFY_EVENT_HEADER_SIZE: usize = std::mem::size_of::<libc::inotify_event>();

pub fn is_skipped_dir_name(name: &str) -> bool {
    SKIP_DIRS.contains(&name)
}

pub fn is_watched_media_file(name: &str, extensions: &HashSet<String>) -> bool {
    if name.is_empty() || name.starts_with('.') || name.contains("@SynoEAStream") {
        return false;
    }
    match Path::new(name).extension().and_then(|e| e.to_str()) {
        Some(ext) => extensions.contains(&ext.to_ascii_lowercase()),
        None => false,
    }
}

pub type EventCallback = Arc<dyn Fn(Option<PathBuf>, &str) + Send + Sync>;

pub struct MediaInotifyWatcher {
    root_dir: PathBuf,
    extensions: HashSet<String>,
    on_media_event: EventCallback,
    fd: i32,
    wake_r: i32,
    wake_w: i32,
    stop_flag: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    watches: Arc<Mutex<WatchMaps>>,
}

#[derive(Default)]
struct WatchMaps {
    wd_to_path: HashMap<i32, Box<str>>,
    path_to_wd: HashMap<Box<str>, i32>,
}

impl MediaInotifyWatcher {
    pub fn new<F>(root_dir: PathBuf, extensions: HashSet<String>, callback: F) -> Self
    where
        F: Fn(Option<PathBuf>, &str) + Send + Sync + 'static,
    {
        Self {
            root_dir,
            extensions,
            on_media_event: Arc::new(callback),
            fd: -1,
            wake_r: -1,
            wake_w: -1,
            stop_flag: Arc::new(AtomicBool::new(false)),
            thread: None,
            watches: Arc::new(Mutex::new(WatchMaps::default())),
        }
    }

    pub fn start(&mut self) -> Result<usize, std::io::Error> {
        if self.thread.is_some() {
            return Ok(self.watches.lock().unwrap().wd_to_path.len());
        }

        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }

        let mut pipe_fds = [-1i32; 2];
        if unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) } < 0 {
            let err = std::io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(err);
        }

        self.fd = fd;
        self.wake_r = pipe_fds[0];
        self.wake_w = pipe_fds[1];
        self.stop_flag.store(false, Ordering::SeqCst);

        add_tree(
            self.fd,
            &self.root_dir,
            &self.watches,
            &self.extensions,
            &self.on_media_event,
            false,
        );

        let watch_count = self.watches.lock().unwrap().wd_to_path.len();

        let fd_copy = self.fd;
        let wake_r_copy = self.wake_r;
        let stop_flag = Arc::clone(&self.stop_flag);
        let watches = Arc::clone(&self.watches);
        let extensions = self.extensions.clone();
        let on_media_event = Arc::clone(&self.on_media_event);

        let handle = thread::Builder::new()
            .name("thumbgen-inotify".into())
            .spawn(move || {
                run_loop(
                    fd_copy,
                    wake_r_copy,
                    stop_flag,
                    watches,
                    extensions,
                    on_media_event,
                );
            })?;

        self.thread = Some(handle);
        Ok(watch_count)
    }

    pub fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if self.wake_w >= 0 {
            let byte = [b'x'];
            unsafe {
                libc::write(self.wake_w, byte.as_ptr().cast(), 1);
            }
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
        for fd_ref in [&mut self.fd, &mut self.wake_r, &mut self.wake_w] {
            if *fd_ref >= 0 {
                unsafe { libc::close(*fd_ref) };
                *fd_ref = -1;
            }
        }
        let mut guard = self.watches.lock().unwrap();
        guard.wd_to_path.clear();
        guard.path_to_wd.clear();
    }
}

impl Drop for MediaInotifyWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

fn add_watch(fd: i32, path: &Path, watches: &Arc<Mutex<WatchMaps>>) {
    let path_str = match path.to_str() {
        Some(s) => s,
        None => return,
    };

    {
        let guard = watches.lock().unwrap();
        if guard.path_to_wd.contains_key(path_str) {
            return;
        }
    }

    let c_path = match CString::new(path.as_os_str().as_bytes()) {
        Ok(c) => c,
        Err(_) => return,
    };

    let wd = unsafe { libc::inotify_add_watch(fd, c_path.as_ptr(), WATCH_MASK) };
    if wd < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ENOSPC) {
            log_warn!(
                "fs.inotify.max_user_watches reached while watching {}; new dirs will rely on fallback poll",
                path.display()
            );
        }
        return;
    }

    let key: Box<str> = path_str.into();
    let mut guard = watches.lock().unwrap();
    if let Some(old_path) = guard.wd_to_path.insert(wd, key.clone()) {
        if old_path.as_ref() != key.as_ref() {
            guard.path_to_wd.remove(&old_path);
        }
    }
    guard.path_to_wd.insert(key, wd);
}

fn drop_wd(wd: i32, watches: &Arc<Mutex<WatchMaps>>) {
    let mut guard = watches.lock().unwrap();
    if let Some(path) = guard.wd_to_path.remove(&wd) {
        if guard.path_to_wd.get(&path) == Some(&wd) {
            guard.path_to_wd.remove(&path);
        }
    }
}

fn add_tree(
    fd: i32,
    base: &Path,
    watches: &Arc<Mutex<WatchMaps>>,
    extensions: &HashSet<String>,
    on_media_event: &EventCallback,
    notify_existing: bool,
) {
    if let Some(name) = base.file_name().and_then(|n| n.to_str()) {
        if is_skipped_dir_name(name) {
            return;
        }
    }
    if !base.is_dir() {
        return;
    }

    let mut stack = vec![base.to_path_buf()];
    while let Some(dir) = stack.pop() {
        add_watch(fd, &dir, watches);
        let entries = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let file_type = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            let file_name = entry.file_name();
            let name_str = file_name.to_string_lossy();
            if file_type.is_dir() {
                if !is_skipped_dir_name(&name_str) {
                    stack.push(entry.path());
                }
            } else if notify_existing && file_type.is_file() && is_watched_media_file(&name_str, extensions) {
                on_media_event(Some(entry.path()), "new_dir_scan");
            }
        }
    }
}

fn handle_buffer(
    fd: i32,
    buf: &[u8],
    watches: &Arc<Mutex<WatchMaps>>,
    extensions: &HashSet<String>,
    on_media_event: &EventCallback,
) {
    let mut offset = 0usize;
    while offset + INOTIFY_EVENT_HEADER_SIZE <= buf.len() {
        let mut ev: libc::inotify_event = unsafe { std::mem::zeroed() };
        unsafe {
            std::ptr::copy_nonoverlapping(
                buf.as_ptr().add(offset),
                (&mut ev as *mut libc::inotify_event).cast(),
                INOTIFY_EVENT_HEADER_SIZE,
            );
        }
        offset += INOTIFY_EVENT_HEADER_SIZE;
        let len = ev.len as usize;
        if offset + len > buf.len() {
            break;
        }
        let name_bytes = &buf[offset..offset + len];
        offset += len;

        let name = CStr::from_bytes_until_nul(name_bytes)
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_else(|_| String::from_utf8_lossy(name_bytes).trim_end_matches('\0').to_string());

        if (ev.mask & libc::IN_Q_OVERFLOW) != 0 {
            log_warn!("inotify event queue overflow; scheduling sync check");
            on_media_event(None, "overflow");
            continue;
        }

        if (ev.mask & (libc::IN_IGNORED | libc::IN_DELETE_SELF)) != 0 {
            drop_wd(ev.wd, watches);
            continue;
        }

        let parent = {
            let guard = watches.lock().unwrap();
            guard.wd_to_path.get(&ev.wd).map(|s| PathBuf::from(s.as_ref()))
        };
        let Some(parent_dir) = parent else {
            continue;
        };

        if (ev.mask & libc::IN_ISDIR) != 0 {
            if (ev.mask & (libc::IN_CREATE | libc::IN_MOVED_TO)) != 0
                && !name.is_empty()
                && !is_skipped_dir_name(&name)
            {
                add_tree(
                    fd,
                    &parent_dir.join(&name),
                    watches,
                    extensions,
                    on_media_event,
                    true,
                );
            }
        } else if (ev.mask & (libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO)) != 0
            && is_watched_media_file(&name, extensions)
        {
            let reason = if (ev.mask & libc::IN_CLOSE_WRITE) != 0 {
                "close_write"
            } else {
                "moved_to"
            };
            on_media_event(Some(parent_dir.join(&name)), reason);
        }
    }
}

fn run_loop(
    fd: i32,
    wake_r: i32,
    stop_flag: Arc<AtomicBool>,
    watches: Arc<Mutex<WatchMaps>>,
    extensions: HashSet<String>,
    on_media_event: EventCallback,
) {
    let mut buf = [0u8; 65536];
    let mut poll_fds = [
        libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wake_r,
            events: libc::POLLIN,
            revents: 0,
        },
    ];

    while !stop_flag.load(Ordering::SeqCst) {
        poll_fds[0].revents = 0;
        poll_fds[1].revents = 0;
        let ready = unsafe { libc::poll(poll_fds.as_mut_ptr(), 2, -1) };
        if ready < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if stop_flag.load(Ordering::SeqCst) || (poll_fds[1].revents & libc::POLLIN) != 0 {
            break;
        }
        if (poll_fds[0].revents & libc::POLLIN) != 0 {
            loop {
                let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
                if n > 0 {
                    handle_buffer(fd, &buf[..n as usize], &watches, &extensions, &on_media_event);
                } else {
                    if n < 0 {
                        let err = std::io::Error::last_os_error();
                        if err.kind() == std::io::ErrorKind::WouldBlock {
                            break;
                        }
                        if err.kind() == std::io::ErrorKind::Interrupted {
                            continue;
                        }
                        return;
                    }
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_WATCH_EXTENSIONS;
    use std::sync::Condvar;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[test]
    fn test_inotify_watcher_detects_media_ignores_eadir_lrf_jpg_and_watches_new_subdirs() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("thumbgen_inotify_{}_{}", std::process::id(), nanos));
        let photos = root.join("Photos");
        let existing_dir = photos.join("CameraA").join("202501");
        let ea_dir = existing_dir.join("@eaDir");
        fs::create_dir_all(&ea_dir).unwrap();

        let extensions: HashSet<String> = DEFAULT_WATCH_EXTENSIONS.iter().map(|s| s.to_string()).collect();
        let events = Arc::new((Mutex::new(Vec::<(Option<PathBuf>, String)>::new()), Condvar::new()));
        let events_cb = Arc::clone(&events);

        let mut watcher = MediaInotifyWatcher::new(photos.clone(), extensions, move |path, reason| {
            let (lock, cv) = &*events_cb;
            let mut list = lock.lock().unwrap();
            list.push((path, reason.to_string()));
            cv.notify_all();
        });

        let watched_count = watcher.start().unwrap();
        assert_eq!(watched_count, 3); // Photos, CameraA, 202501 (@eaDir excluded)

        // Ignored files: @eaDir, .LRF, .jpg, dotfile
        fs::write(ea_dir.join("SYNOPHOTO_THUMB_XL.jpg"), b"thumb").unwrap();
        fs::write(existing_dir.join("CLIP_0001.LRF"), b"lrf").unwrap();
        fs::write(existing_dir.join("IMG_0001.jpg"), b"jpg").unwrap();
        fs::write(existing_dir.join(".hidden.MP4"), b"hidden").unwrap();

        {
            let (lock, cv) = &*events;
            let guard = lock.lock().unwrap();
            let (guard, timeout) = cv
                .wait_timeout_while(guard, Duration::from_millis(300), |list| list.is_empty())
                .unwrap();
            assert!(timeout.timed_out());
            assert!(guard.is_empty());
        }

        // Valid MP4 triggers close_write
        let mp4_path = existing_dir.join("CLIP_0001.MP4");
        fs::write(&mp4_path, b"mp4data").unwrap();
        {
            let (lock, cv) = &*events;
            let guard = lock.lock().unwrap();
            let (mut guard, timeout) = cv
                .wait_timeout_while(guard, Duration::from_secs(2), |list| list.is_empty())
                .unwrap();
            assert!(!timeout.timed_out());
            assert!(guard
                .iter()
                .any(|(p, r)| p.as_ref() == Some(&mp4_path) && r == "close_write"));
            guard.clear();
        }

        // Brand-new nested subdirectory + HEIC file triggers dynamically
        let new_subdir = photos.join("CameraA").join("202502");
        fs::create_dir(&new_subdir).unwrap();
        let heic_path = new_subdir.join("IMG_0001.HEIC");
        fs::write(&heic_path, b"heicdata").unwrap();
        {
            let (lock, cv) = &*events;
            let guard = lock.lock().unwrap();
            let (guard, timeout) = cv
                .wait_timeout_while(guard, Duration::from_secs(2), |list| {
                    !list.iter().any(|(p, _)| p.as_ref() == Some(&heic_path))
                })
                .unwrap();
            assert!(!timeout.timed_out());
            assert!(guard.iter().any(|(p, _)| p.as_ref() == Some(&heic_path)));
        }

        watcher.stop();
        let _ = fs::remove_dir_all(root);
    }
}
