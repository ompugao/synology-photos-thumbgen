mod config;
mod dsm_client;
mod log;
mod media;
mod state;
mod watcher;

use config::Settings;
use dsm_client::{ConvertItem, DsmError, DsmPhotosClient};
use media::{generate_thumbnails, read_jpeg_dimensions, LocalMediaResolver};
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use state::StateStore;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use watcher::MediaInotifyWatcher;

enum CliCommand {
    Daemon,
    Once { dry_run: bool, limit: Option<usize> },
    Auth,
    TestLocal { file: PathBuf, out_dir: Option<PathBuf> },
    Help,
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(prefix: &str) -> io::Result<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = env::temp_dir().join(format!("{prefix}{}_{}", std::process::id(), nanos));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[derive(Default)]
struct CycleStats {
    processed: usize,
    succeeded: usize,
    failed: usize,
    skipped: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TriggerKind {
    Startup,
    Inotify,
    Followup,
    FallbackPoll,
}

impl TriggerKind {
    fn as_str(self) -> &'static str {
        match self {
            TriggerKind::Startup => "startup",
            TriggerKind::Inotify => "inotify",
            TriggerKind::Followup => "followup",
            TriggerKind::FallbackPoll => "fallback_poll",
        }
    }
}

struct DaemonSchedule {
    pending_event_at: Option<Instant>,
    last_event_log_at: Option<Instant>,
    suppressed_event_count: usize,
    followup_due_at: Option<Instant>,
}

fn validate_credentials(settings: &Settings) -> bool {
    if settings.dsm_username.is_empty() || settings.dsm_password.is_empty() {
        log_error!(
            "DSM_USERNAME or DSM_PASSWORD is empty. Set DSM_USERNAME and DSM_PASSWORD in .env before running."
        );
        return false;
    }
    true
}

fn resolve_or_download(
    client: &mut DsmPhotosClient,
    resolver: &mut LocalMediaResolver,
    item: &ConvertItem,
    tmp_dir: &Path,
    allow_download_fallback: bool,
) -> Result<(PathBuf, &'static str), DsmError> {
    let folder_rel = client.resolve_folder_path(item.unit_id)?;
    if let Some(local_path) = resolver.resolve(&item.filename, folder_rel.as_deref()) {
        return Ok((local_path, "local"));
    }
    if !allow_download_fallback {
        return Err(DsmError::Io(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "Local file not found for unit_id={} filename={} (folder={:?}) and ALLOW_DOWNLOAD_FALLBACK=false",
                item.unit_id, item.filename, folder_rel
            ),
        )));
    }
    let download_path = tmp_dir.join(&item.filename);
    log_info!(
        "Local file not matched for {} ({}); downloading via API",
        item.filename,
        item.unit_id
    );
    client.download_unit(item.unit_id, &download_path)?;
    Ok((download_path, "download"))
}

fn run_cycle(
    client: &mut DsmPhotosClient,
    resolver: &mut LocalMediaResolver,
    state: &mut StateStore,
    settings: &Settings,
    dry_run: bool,
    limit: Option<usize>,
) -> Result<CycleStats, DsmError> {
    let mut stats = CycleStats::default();
    let mut seen_ids = HashSet::new();

    loop {
        if let Some(max_items) = limit {
            if stats.processed >= max_items {
                break;
            }
        }

        let pending = client.list_convert_needed()?;
        let batch: Vec<ConvertItem> = pending
            .into_iter()
            .filter(|item| !seen_ids.contains(&item.unit_id))
            .collect();
        if batch.is_empty() {
            break;
        }

        for item in batch {
            if let Some(max_items) = limit {
                if stats.processed >= max_items {
                    break;
                }
            }

            seen_ids.insert(item.unit_id);

            if state.should_skip(item.unit_id) {
                stats.skipped += 1;
                log_debug!("Skipping unit_id={} ({}) in backoff", item.unit_id, item.filename);
                continue;
            }

            let filename = if item.filename.is_empty() {
                format!("unit_{}", item.unit_id)
            } else {
                item.filename.clone()
            };

            if dry_run {
                let folder_rel = client.resolve_folder_path(item.unit_id)?;
                let local_path = resolver.resolve(&filename, folder_rel.as_deref());
                let source_desc = local_path
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<download-fallback>".to_string());
                log_info!(
                    "[DRY-RUN] unit_id={} type={} need_thumbnail={} need_video={} filename={} source={}",
                    item.unit_id,
                    item.item_type,
                    item.need_thumbnail,
                    item.need_video,
                    filename,
                    source_desc
                );
                stats.processed += 1;
                stats.succeeded += 1;
                continue;
            }

            stats.processed += 1;
            let t0 = Instant::now();
            let item_res: Result<(), DsmError> = (|| {
                let tmp = TempDir::new(&format!("thumb_{}_", item.unit_id))?;
                let (src_path, source_kind) = resolve_or_download(
                    client,
                    resolver,
                    &item,
                    &tmp.path,
                    settings.download_fallback,
                )?;
                let out_dir = tmp.path.join("out");
                let (thumb_sm, thumb_m, thumb_xl) = generate_thumbnails(
                    &src_path,
                    Some(item.item_type),
                    &out_dir,
                    settings,
                )
                .map_err(|e| DsmError::Io(io::Error::other(e)))?;

                client.upload_thumbnails(item.unit_id, &thumb_sm, &thumb_m, &thumb_xl)?;

                let elapsed_ms = t0.elapsed().as_millis();
                let sm_len = fs::metadata(&thumb_sm)?.len();
                let m_len = fs::metadata(&thumb_m)?.len();
                let xl_len = fs::metadata(&thumb_xl)?.len();
                log_info!(
                    "Uploaded thumbnails for {} (unit_id={}, source={}) in {} ms [sm={}B m={}B xl={}B]",
                    filename,
                    item.unit_id,
                    source_kind,
                    elapsed_ms,
                    sm_len,
                    m_len,
                    xl_len
                );
                Ok(())
            })();

            match item_res {
                Ok(()) => {
                    if let Err(err) = state.clear_failure(item.unit_id) {
                        log_warn!("Failed clearing backoff state for {}: {}", item.unit_id, err);
                    }
                    stats.succeeded += 1;
                }
                Err(err @ DsmError::SessionExpired(_)) => {
                    resolver.clear_index();
                    client.clear_folder_cache();
                    return Err(err);
                }
                Err(err) => {
                    stats.failed += 1;
                    let err_str = err.to_string();
                    match state.record_failure(item.unit_id, &err_str) {
                        Ok(retry_after) => {
                            let delay = (retry_after - state::now_unix_secs()).max(0.0) as u64;
                            log_warn!(
                                "Failed unit_id={} ({}): {} (next retry in {}s)",
                                item.unit_id,
                                filename,
                                err_str,
                                delay
                            );
                        }
                        Err(state_err) => {
                            log_warn!(
                                "Failed unit_id={} ({}): {} (and failed saving state: {})",
                                item.unit_id,
                                filename,
                                err_str,
                                state_err
                            );
                        }
                    }
                    if settings.mark_broken_on_error {
                        if let Err(broken_err) = client.set_broken(item.unit_id) {
                            log_warn!(
                                "Failed calling set_broken on unit_id={}: {}",
                                item.unit_id,
                                broken_err
                            );
                        }
                    }
                }
            }
        }
    }

    resolver.clear_index();
    client.clear_folder_cache();

    Ok(stats)
}

fn run_cycle_with_reauth(
    client: &mut DsmPhotosClient,
    resolver: &mut LocalMediaResolver,
    state: &mut StateStore,
    settings: &Settings,
    dry_run: bool,
    limit: Option<usize>,
) -> Result<CycleStats, DsmError> {
    if client.sid.is_none() {
        client.login(state, None)?;
    }
    let res = match run_cycle(client, resolver, state, settings, dry_run, limit) {
        Ok(stats) => Ok(stats),
        Err(DsmError::SessionExpired(_)) => {
            log_info!("DSM session expired; re-authenticating once");
            client.sid = None;
            client.syno_token = None;
            client.login(state, None)?;
            run_cycle(client, resolver, state, settings, dry_run, limit)
        }
        Err(err) => Err(err),
    };
    client.logout();
    res
}

fn cmd_once(settings: &Settings, dry_run: bool, limit: Option<usize>) -> i32 {
    if !validate_credentials(settings) {
        return 2;
    }
    let mut state = match StateStore::load(settings.data_dir.join("state.json")) {
        Ok(s) => s,
        Err(err) => {
            log_error!("{}", err);
            return 1;
        }
    };
    let mut resolver = LocalMediaResolver::new(settings.photos_dir.clone());
    let mut client = DsmPhotosClient::new(settings.clone(), state.device_id.clone());

    let code = match client.login(&mut state, None) {
        Ok(_) => match run_cycle_with_reauth(
            &mut client,
            &mut resolver,
            &mut state,
            settings,
            dry_run,
            limit,
        ) {
            Ok(stats) => {
                log_info!(
                    "Cycle complete: processed={} succeeded={} failed={} skipped={}",
                    stats.processed,
                    stats.succeeded,
                    stats.failed,
                    stats.skipped
                );
                0
            }
            Err(DsmError::AuthFatal { requires_otp, message, .. }) => {
                log_error!("{}", message);
                if requires_otp {
                    log_error!("2FA is enabled. Run `docker compose run --rm thumbgen auth` once.");
                }
                2
            }
            Err(err) => {
                log_error!("Cycle failed: {}", err);
                1
            }
        },
        Err(DsmError::AuthFatal { requires_otp, message, .. }) => {
            log_error!("{}", message);
            if requires_otp {
                log_error!("2FA is enabled. Run `docker compose run --rm thumbgen auth` once.");
            }
            2
        }
        Err(err) => {
            log_error!("Login failed: {}", err);
            1
        }
    };

    client.logout();
    code
}

fn spawn_signal_watcher(
    stop_flag: Arc<AtomicBool>,
    schedule: Arc<(Mutex<DaemonSchedule>, Condvar)>,
) {
    if let Ok(mut signals) = Signals::new([SIGINT, SIGTERM]) {
        thread::Builder::new()
            .name("thumbgen-signals".into())
            .spawn(move || {
                for sig in signals.forever() {
                    log_info!("Received signal {}; shutting down", sig);
                    stop_flag.store(true, Ordering::SeqCst);
                    let (_lock, cv) = &*schedule;
                    cv.notify_all();
                    break;
                }
            })
            .ok();
    }
}

fn cmd_daemon(settings: &Settings) -> i32 {
    if !validate_credentials(settings) {
        return 2;
    }

    let mut state = match StateStore::load(settings.data_dir.join("state.json")) {
        Ok(s) => s,
        Err(err) => {
            log_error!("{}", err);
            return 1;
        }
    };
    let mut resolver = LocalMediaResolver::new(settings.photos_dir.clone());
    let mut client = DsmPhotosClient::new(settings.clone(), state.device_id.clone());

    if let Err(err) = client.login(&mut state, None) {
        match err {
            DsmError::AuthFatal { requires_otp, message, .. } => {
                log_error!("{}", message);
                if requires_otp {
                    log_error!("2FA is enabled. Run `docker compose run --rm thumbgen auth` once.");
                }
                return 2;
            }
            other => {
                log_error!("Initial login failed: {}", other);
                return 1;
            }
        }
    }

    let stop_flag = Arc::new(AtomicBool::new(false));
    let schedule = Arc::new((
        Mutex::new(DaemonSchedule {
            pending_event_at: None,
            last_event_log_at: None,
            suppressed_event_count: 0,
            followup_due_at: None,
        }),
        Condvar::new(),
    ));
    spawn_signal_watcher(Arc::clone(&stop_flag), Arc::clone(&schedule));

    let mut watcher: Option<MediaInotifyWatcher> = None;
    if settings.watch_enabled && settings.photos_dir.exists() {
        let sched_cb = Arc::clone(&schedule);
        let debounce_dur = Duration::from_secs(settings.watch_debounce_seconds);
        let mut w = MediaInotifyWatcher::new(
            settings.photos_dir.clone(),
            settings.watch_extensions.clone(),
            move |path, reason| {
                let now = Instant::now();
                let (should_log, prev_suppressed) = {
                    let (lock, cv) = &*sched_cb;
                    let mut sched = lock.lock().unwrap();
                    if sched.pending_event_at.is_none() {
                        sched.pending_event_at = Some(now);
                    }
                    sched.followup_due_at = None;
                    let can_log = match sched.last_event_log_at {
                        Some(prev) => now.duration_since(prev) >= debounce_dur,
                        None => true,
                    };
                    let res = if can_log {
                        sched.last_event_log_at = Some(now);
                        let suppressed = sched.suppressed_event_count;
                        sched.suppressed_event_count = 0;
                        (true, suppressed)
                    } else {
                        sched.suppressed_event_count += 1;
                        (false, 0)
                    };
                    cv.notify_all();
                    res
                };
                if should_log {
                    let target = path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "<overflow>".to_string());
                    if prev_suppressed > 0 {
                        log_info!(
                            "Detected media change ({}): {} (+{} more in previous window)",
                            reason,
                            target,
                            prev_suppressed
                        );
                    } else {
                        log_info!("Detected media change ({}): {}", reason, target);
                    }
                }
            },
        );
        match w.start() {
            Ok(watch_count) => {
                log_info!(
                    "Inotify watcher started ({} directories under {}, debounce={}s, followup={}s, fallback_poll={}s)",
                    watch_count,
                    settings.photos_dir.display(),
                    settings.watch_debounce_seconds,
                    settings.watch_followup_seconds,
                    settings.poll_interval_seconds
                );
                watcher = Some(w);
            }
            Err(err) => {
                log_warn!(
                    "Failed starting inotify watcher on {} ({}); falling back to periodic polling",
                    settings.photos_dir.display(),
                    err
                );
            }
        }
    } else if settings.watch_enabled {
        log_warn!(
            "PHOTOS_DIR does not exist ({}); inotify watcher disabled",
            settings.photos_dir.display()
        );
    }

    log_info!(
        "Daemon started (poll_interval={}s, photos_dir={}, watch_enabled={})",
        settings.poll_interval_seconds,
        settings.photos_dir.display(),
        settings.watch_enabled
    );

    let debounce_dur = Duration::from_secs(settings.watch_debounce_seconds);
    let poll_dur = Duration::from_secs(settings.poll_interval_seconds);
    let followup_dur = Duration::from_secs(settings.watch_followup_seconds);

    let mut next_fallback_poll_at = Instant::now();
    let mut initial_cycle_pending = true;
    let mut exit_code = 0;

    while !stop_flag.load(Ordering::SeqCst) {
        let trigger = {
            let (lock, cv) = &*schedule;
            let mut sched = lock.lock().unwrap();
            loop {
                if stop_flag.load(Ordering::SeqCst) {
                    break None;
                }
                let now = Instant::now();
                let mut next_due = next_fallback_poll_at;
                let mut best_trigger = if initial_cycle_pending {
                    TriggerKind::Startup
                } else {
                    TriggerKind::FallbackPoll
                };

                if let Some(ev_at) = sched.pending_event_at {
                    let ev_due = ev_at + debounce_dur;
                    if ev_due < next_due {
                        next_due = ev_due;
                        best_trigger = TriggerKind::Inotify;
                    }
                }
                if let Some(fu_due) = sched.followup_due_at {
                    if fu_due < next_due {
                        next_due = fu_due;
                        best_trigger = TriggerKind::Followup;
                    }
                }

                if now >= next_due {
                    match best_trigger {
                        TriggerKind::Inotify => {
                            sched.pending_event_at = None;
                            sched.suppressed_event_count = 0;
                            sched.followup_due_at = None;
                        }
                        TriggerKind::Followup => {
                            sched.followup_due_at = None;
                        }
                        TriggerKind::Startup | TriggerKind::FallbackPoll => {
                            sched.pending_event_at = None;
                            sched.suppressed_event_count = 0;
                            sched.followup_due_at = None;
                        }
                    }
                    break Some(best_trigger);
                }

                let wait_dur = next_due.saturating_duration_since(now);
                let (new_sched, _) = cv.wait_timeout(sched, wait_dur).unwrap();
                sched = new_sched;
            }
        };

        let Some(trigger) = trigger else {
            break;
        };

        initial_cycle_pending = false;
        match run_cycle_with_reauth(
            &mut client,
            &mut resolver,
            &mut state,
            settings,
            false,
            None,
        ) {
            Ok(stats) => {
                if stats.processed > 0 || stats.skipped > 0 || trigger != TriggerKind::FallbackPoll {
                    log_info!(
                        "Cycle complete ({}): processed={} succeeded={} failed={} skipped={}",
                        trigger.as_str(),
                        stats.processed,
                        stats.succeeded,
                        stats.failed,
                        stats.skipped
                    );
                }
                next_fallback_poll_at = Instant::now() + poll_dur;
                if trigger == TriggerKind::Inotify
                    && stats.processed == 0
                    && settings.watch_followup_seconds > 0
                {
                    let (lock, _) = &*schedule;
                    let mut sched = lock.lock().unwrap();
                    if sched.pending_event_at.is_none() {
                        sched.followup_due_at = Some(Instant::now() + followup_dur);
                        log_info!(
                            "No pending items yet after media event; scheduling follow-up check in {}s",
                            settings.watch_followup_seconds
                        );
                    }
                }
            }
            Err(DsmError::AuthFatal { requires_otp, message, .. }) => {
                log_error!("Stopping daemon to prevent DSM Auto Block: {}", message);
                if requires_otp {
                    log_error!("Run `docker compose run --rm thumbgen auth` to refresh device token.");
                }
                exit_code = 2;
                break;
            }
            Err(err) => {
                log_error!("Poll cycle error (will retry next interval): {}", err);
                next_fallback_poll_at = Instant::now() + Duration::from_secs(settings.poll_interval_seconds.min(300));
            }
        }
    }

    if let Some(mut w) = watcher {
        w.stop();
    }
    client.logout();
    exit_code
}

struct EchoGuard {
    orig: Option<libc::termios>,
}

impl EchoGuard {
    fn disable_echo() -> Self {
        unsafe {
            if libc::isatty(libc::STDIN_FILENO) != 1 {
                return Self { orig: None };
            }
            let mut term: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut term) != 0 {
                return Self { orig: None };
            }
            let orig = term;
            term.c_lflag &= !libc::ECHO;
            let _ = libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &term);
            Self { orig: Some(orig) }
        }
    }
}

impl Drop for EchoGuard {
    fn drop(&mut self) {
        if let Some(orig) = self.orig {
            unsafe {
                let _ = libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &orig);
            }
        }
    }
}

fn read_line_prompt(prompt: &str, secret: bool) -> io::Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    let _guard = if secret { Some(EchoGuard::disable_echo()) } else { None };
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    if secret {
        println!();
    }
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

fn cmd_auth(settings: &Settings) -> i32 {
    if settings.dsm_username.is_empty() {
        log_error!("DSM_USERNAME is empty.");
        return 2;
    }

    let password = if !settings.dsm_password.is_empty() {
        settings.dsm_password.clone()
    } else {
        match read_line_prompt(
            &format!("DSM password for {}: ", settings.dsm_username),
            true,
        ) {
            Ok(pw) => pw,
            Err(err) => {
                log_error!("Failed reading password: {}", err);
                return 1;
            }
        }
    };
    if password.is_empty() {
        log_error!("DSM password cannot be empty.");
        return 2;
    }

    let mut auth_settings = settings.clone();
    auth_settings.dsm_password = password;

    let mut state = match StateStore::load(auth_settings.data_dir.join("state.json")) {
        Ok(s) => s,
        Err(err) => {
            log_error!("{}", err);
            return 1;
        }
    };
    let mut client = DsmPhotosClient::new(auth_settings, state.device_id.clone());

    let code = match client.login(&mut state, None) {
        Ok(did) => match client.list_convert_needed() {
            Ok(pending) => {
                log_info!(
                    "Authentication succeeded (device_id saved={}). Pending thumbnails visible now: {}",
                    did.is_some(),
                    pending.len()
                );
                0
            }
            Err(err) => {
                log_error!("Authenticated, but list_convert_needed failed: {}", err);
                1
            }
        },
        Err(DsmError::AuthFatal { requires_otp: true, .. }) => {
            let otp = match read_line_prompt("2FA OTP code: ", false) {
                Ok(v) => v.trim().to_string(),
                Err(err) => {
                    log_error!("Failed reading OTP code: {}", err);
                    client.logout();
                    return 1;
                }
            };
            if otp.is_empty() {
                log_error!("OTP code is required.");
                client.logout();
                return 2;
            }
            match client.login(&mut state, Some(&otp)) {
                Ok(did) => match client.list_convert_needed() {
                    Ok(pending) => {
                        log_info!(
                            "Authentication succeeded (device_id saved={}). Pending thumbnails visible now: {}",
                            did.is_some(),
                            pending.len()
                        );
                        0
                    }
                    Err(err) => {
                        log_error!("Authenticated, but list_convert_needed failed: {}", err);
                        1
                    }
                },
                Err(err) => {
                    log_error!("{}", err);
                    2
                }
            }
        }
        Err(err) => {
            log_error!("{}", err);
            2
        }
    };

    client.logout();
    code
}

fn cmd_test_local(settings: &Settings, file_path: &Path, out_dir: Option<&Path>) -> i32 {
    if !file_path.is_file() {
        log_error!("File not found: {}", file_path.display());
        return 1;
    }

    let run_into = |dest: &Path| -> i32 {
        let t0 = Instant::now();
        let (thumb_sm, thumb_m, thumb_xl) = match generate_thumbnails(file_path, None, dest, settings) {
            Ok(paths) => paths,
            Err(err) => {
                log_error!("Thumbnail generation failed for {}: {}", file_path.display(), err);
                return 1;
            }
        };
        let elapsed_ms = t0.elapsed().as_millis();

        for (label, path) in [
            ("thumb_xl", &thumb_xl),
            ("thumb_m", &thumb_m),
            ("thumb_sm", &thumb_sm),
        ] {
            let dims = match read_jpeg_dimensions(path) {
                Ok(d) => d,
                Err(err) => {
                    log_error!("Generated invalid JPEG {}: {}", path.display(), err);
                    return 1;
                }
            };
            let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            println!("{label}: {}x{} ({size} bytes) -> {}", dims.0, dims.1, path.display());
        }
        println!("elapsed_ms: {elapsed_ms}");
        0
    };

    if let Some(dest) = out_dir {
        return run_into(dest);
    }

    match TempDir::new("thumbgen_test_") {
        Ok(tmp) => run_into(&tmp.path),
        Err(err) => {
            log_error!("Failed creating temp directory: {}", err);
            1
        }
    }
}

fn print_help() {
    println!(
        "Usage: synology-thumbgen <command> [options]\n\n\
         Commands:\n  \
           daemon                        Watch /photos via inotify and poll DSM periodically (default)\n  \
           once [--dry-run] [--limit N]  Run a single polling cycle and exit\n  \
           auth                          Interactive login to save 2FA device_id in state.json\n  \
           test-local <file> [--out-dir <dir>]  Offline thumbnail extraction check on a local file"
    );
}

fn parse_args() -> Result<CliCommand, String> {
    let mut args = env::args().skip(1);
    let Some(cmd) = args.next() else {
        return Ok(CliCommand::Daemon);
    };
    match cmd.as_str() {
        "-h" | "--help" => Ok(CliCommand::Help),
        "daemon" => {
            if let Some(arg) = args.next() {
                if arg == "-h" || arg == "--help" {
                    return Ok(CliCommand::Help);
                }
                return Err(format!("Unexpected argument for daemon: {arg}"));
            }
            Ok(CliCommand::Daemon)
        }
        "once" => {
            let mut dry_run = false;
            let mut limit = None;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "-h" | "--help" => return Ok(CliCommand::Help),
                    "--dry-run" => dry_run = true,
                    "--limit" => {
                        let val = args.next().ok_or_else(|| "--limit requires a value".to_string())?;
                        let parsed: usize = val
                            .parse()
                            .map_err(|e| format!("Invalid --limit value '{val}': {e}"))?;
                        limit = Some(parsed);
                    }
                    other => return Err(format!("Unknown argument for once: {other}")),
                }
            }
            Ok(CliCommand::Once { dry_run, limit })
        }
        "auth" => {
            if let Some(arg) = args.next() {
                if arg == "-h" || arg == "--help" {
                    return Ok(CliCommand::Help);
                }
                return Err(format!("Unexpected argument for auth: {arg}"));
            }
            Ok(CliCommand::Auth)
        }
        "test-local" => {
            let mut file: Option<PathBuf> = None;
            let mut out_dir: Option<PathBuf> = None;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "-h" | "--help" => return Ok(CliCommand::Help),
                    "--out-dir" => {
                        let val = args
                            .next()
                            .ok_or_else(|| "--out-dir requires a directory path".to_string())?;
                        out_dir = Some(PathBuf::from(val));
                    }
                    other if other.starts_with('-') => {
                        return Err(format!("Unknown option for test-local: {other}"));
                    }
                    other => {
                        if file.is_some() {
                            return Err(format!("Unexpected extra argument: {other}"));
                        }
                        file = Some(PathBuf::from(other));
                    }
                }
            }
            let file = file.ok_or_else(|| "test-local requires <file>".to_string())?;
            Ok(CliCommand::TestLocal { file, out_dir })
        }
        other => Err(format!("Unknown command: {other}")),
    }
}

fn main() {
    let command = match parse_args() {
        Ok(cmd) => cmd,
        Err(err) => {
            eprintln!("Error: {err}");
            print_help();
            std::process::exit(2);
        }
    };

    if matches!(command, CliCommand::Help) {
        print_help();
        return;
    }

    let settings = match Settings::from_env() {
        Ok(s) => s,
        Err(err) => {
            eprintln!("Configuration error: {err}");
            std::process::exit(2);
        }
    };
    log::init_logger(&settings.log_level);

    let exit_code = match command {
        CliCommand::Daemon => cmd_daemon(&settings),
        CliCommand::Once { dry_run, limit } => cmd_once(&settings, dry_run, limit),
        CliCommand::Auth => cmd_auth(&settings),
        CliCommand::TestLocal { file, out_dir } => {
            cmd_test_local(&settings, &file, out_dir.as_deref())
        }
        CliCommand::Help => 0,
    };

    std::process::exit(exit_code);
}
