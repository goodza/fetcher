//! Opt-in, fail-open cookie refresh over a private loopback tunnel.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const MAX_BODY: usize = 2 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(10);
const SUCCESS_TTL: Duration = Duration::from_secs(600);
const FAILURE_BACKOFF: Duration = Duration::from_secs(60);
const HEADER: &str = "# Netscape HTTP Cookie File";
static STATE: Mutex<RefreshState> = Mutex::const_new(RefreshState::new());

struct Config {
    url: String,
    token_file: PathBuf,
    cookies_file: PathBuf,
}

struct RefreshState {
    next_attempt: Option<Instant>,
    failure_until: Option<Instant>,
    last_success: Option<Instant>,
    last_error_attempt: Option<Instant>,
}

impl RefreshState {
    const fn new() -> Self {
        Self {
            next_attempt: None,
            failure_until: None,
            last_success: None,
            last_error_attempt: None,
        }
    }

    async fn refresh(&mut self, config: &Config, timeout: Duration) -> Option<bool> {
        if self.next_attempt.is_some_and(|next| Instant::now() < next) {
            return None;
        }
        Some(self.update(config, timeout).await)
    }

    async fn refresh_after_error(
        &mut self,
        config: &Config,
        timeout: Duration,
        started: Instant,
    ) -> bool {
        // Check before cooldowns: all jobs using the superseded cookies may retry.
        // A cached pre-work refresh never advances last_success.
        if self.last_success.is_some_and(|updated| updated > started) {
            return true;
        }
        let now = Instant::now();
        if self.failure_until.is_some_and(|until| now < until)
            || self
                .last_error_attempt
                .is_some_and(|last| now.duration_since(last) < FAILURE_BACKOFF)
        {
            return false;
        }
        // Separate from success TTL; also survives cancellation during the request.
        self.last_error_attempt = Some(now);
        self.update(config, timeout).await
    }

    async fn update(&mut self, config: &Config, timeout: Duration) -> bool {
        // Set both guards BEFORE awaiting, so cancellation remains fail-open/backed off.
        let until = Instant::now() + FAILURE_BACKOFF;
        self.next_attempt = Some(until);
        self.failure_until = Some(until);
        let success = refresh_once(config, timeout).await.is_ok();
        let now = Instant::now();
        if success {
            self.last_success = Some(now);
            self.failure_until = None;
        } else {
            self.failure_until = Some(now + FAILURE_BACKOFF);
        }
        self.next_attempt = Some(
            now + if success {
                SUCCESS_TTL
            } else {
                FAILURE_BACKOFF
            },
        );
        success
    }
}

fn configured() -> Option<Config> {
    Some(Config {
        url: std::env::var_os("COOKIE_REFRESH_URL")?
            .into_string()
            .unwrap_or_default(),
        token_file: std::env::var_os("COOKIE_REFRESH_TOKEN_FILE")
            .map(PathBuf::from)
            .unwrap_or_default(),
        // Match the downloader's existing lookup; bootstrap into cwd if absent.
        cookies_file: super::cookie_file_path().unwrap_or_else(|| PathBuf::from("cookies.txt")),
    })
}

/// Disabled unless explicitly configured; failure never prevents a download.
/// Keep the lock across I/O so simultaneous callers share one refresh attempt.
pub(super) async fn refresh_if_configured() {
    let Some(config) = configured() else {
        return;
    };
    match STATE.lock().await.refresh(&config, TIMEOUT).await {
        Some(true) => log::info!("Cookies refreshed from PC webhook"),
        // Never include reqwest errors, endpoint, token path or response contents.
        Some(false) => {
            log::warn!("Cookie refresh unavailable or rejected; retaining existing cookies");
        }
        None => {}
    }
}

pub(super) async fn refresh_after_error(started: Instant) -> bool {
    let Some(config) = configured() else {
        return false;
    };
    STATE
        .lock()
        .await
        .refresh_after_error(&config, TIMEOUT, started)
        .await
}

/// Shared bounded retry policy for buffered AND progress-streamed yt-dlp commands.
/// Only an actual unsuccessful exit qualifies, never a spawn/read/wait/timeout error.
pub(super) async fn run_with_retry<T, E, F, Fut>(use_cookies: bool, attempt: F) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(std::process::ExitStatus, T), E>>,
{
    run_with_refresh(use_cookies, attempt, refresh_after_error).await
}

async fn run_with_refresh<T, E, F, Fut, R, RFut>(
    use_cookies: bool,
    mut attempt: F,
    mut refresh: R,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(std::process::ExitStatus, T), E>>,
    R: FnMut(Instant) -> RFut,
    RFut: std::future::Future<Output = bool>,
{
    // Callers have completed normal TTL refresh before entering this wrapper.
    let started = Instant::now();
    let (status, output) = attempt().await?;
    if status.success() || !use_cookies {
        return Ok(output);
    }
    if !refresh(started).await {
        return Ok(output);
    }
    log::info!("yt-dlp failed; refreshed cookies, retrying once");
    let (_, output) = attempt().await?;
    Ok(output)
}

pub(super) async fn run_output<F>(
    command: F,
    use_cookies: bool,
    deadline: Option<Duration>,
) -> std::io::Result<std::process::Output>
where
    F: FnMut() -> tokio::process::Command,
{
    run_output_with_refresh(command, use_cookies, deadline, refresh_after_error).await
}

async fn run_output_with_refresh<F, R, RFut>(
    mut command: F,
    use_cookies: bool,
    deadline: Option<Duration>,
    refresh: R,
) -> std::io::Result<std::process::Output>
where
    F: FnMut() -> tokio::process::Command,
    R: FnMut(Instant) -> RFut,
    RFut: std::future::Future<Output = bool>,
{
    run_with_refresh(
        use_cookies,
        || {
            let mut command = command();
            command.kill_on_drop(true);
            async move {
                let output = if let Some(deadline) = deadline {
                    tokio::time::timeout(deadline, command.output())
                        .await
                        .map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))??
                } else {
                    command.output().await?
                };
                Ok((output.status, output))
            }
        },
        refresh,
    )
    .await
}

fn loopback_url(value: &str) -> Result<reqwest::Url, ()> {
    let url = reqwest::Url::parse(value).map_err(|_| ())?;
    let host = url.host_str().ok_or(())?;
    let is_loopback = host == "localhost"
        || host
            .trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if url.scheme() != "http"
        || !is_loopback
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(());
    }
    Ok(url)
}

async fn refresh_once(config: &Config, timeout: Duration) -> Result<(), ()> {
    let url = loopback_url(&config.url)?;
    let token = read_limited(&config.token_file, 8192)?;
    let token = std::str::from_utf8(&token)
        .map_err(|_| ())?
        .trim_end_matches(['\r', '\n']);
    if token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(());
    }
    let mut authorization =
        reqwest::header::HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| ())?;
    authorization.set_sensitive(true);
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        // Do not trust DNS (or hosts-file overrides) for the localhost alias.
        .resolve("localhost", SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
        .timeout(timeout)
        .build()
        .map_err(|_| ())?;
    let mut response = client
        .post(url)
        .header(reqwest::header::AUTHORIZATION, authorization)
        .send()
        .await
        .map_err(|_| ())?;
    if response.status() != reqwest::StatusCode::OK
        || response
            .content_length()
            .is_some_and(|length| length > MAX_BODY as u64)
    {
        return Err(());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
        if chunk.len() > MAX_BODY - body.len() {
            return Err(());
        }
        body.extend_from_slice(&chunk);
    }
    let body = String::from_utf8(body).map_err(|_| ())?;
    let path = config.cookies_file.clone();
    tokio::task::spawn_blocking(move || install_cookies(&path, &body))
        .await
        .map_err(|_| ())?
}

fn read_limited(path: &Path, limit: usize) -> Result<Vec<u8>, ()> {
    let mut contents = Vec::new();
    File::open(path)
        .map_err(|_| ())?
        .take(limit as u64 + 1)
        .read_to_end(&mut contents)
        .map_err(|_| ())?;
    if contents.len() > limit {
        return Err(());
    }
    Ok(contents)
}

fn relevant_domain(domain: &str) -> bool {
    let domain = domain.strip_prefix('.').unwrap_or(domain);
    ["instagram.com", "youtube.com", "google.com"]
        .iter()
        .any(|base| {
            domain == *base
                || domain
                    .strip_suffix(base)
                    .is_some_and(|prefix| prefix.ends_with('.'))
        })
}

fn cookie_fields(row: &str) -> Result<Vec<&str>, ()> {
    let row = row.strip_prefix("#HttpOnly_").unwrap_or(row);
    let fields: Vec<_> = row.split('\t').collect();
    if fields.len() != 7 || row.chars().any(|c| c.is_control() && c != '\t') {
        return Err(());
    }
    let domain = fields[0].strip_prefix('.').unwrap_or(fields[0]);
    if domain.is_empty()
        || !domain.split('.').all(|label| {
            !label.is_empty()
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
        || !matches!(fields[1], "TRUE" | "FALSE")
        || (fields[1] == "TRUE") != fields[0].starts_with('.')
        || !matches!(fields[3], "TRUE" | "FALSE")
        || !fields[2].starts_with('/')
        || fields[4].parse::<u64>().is_err()
        || fields[5].is_empty()
        || !fields[5]
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && !b"()<>@,;:\\\"/[]?={}".contains(&byte))
        || !fields[6]
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && !b";\"\\".contains(&byte))
    {
        return Err(());
    }
    Ok(fields)
}

fn validated_rows(contents: &str) -> Result<Vec<&str>, ()> {
    if contents.len() > MAX_BODY
        || !contents.ends_with('\n')
        || contents.contains('\r')
        || contents.contains('\0')
    {
        return Err(());
    }
    let mut lines = contents.split('\n');
    if lines.next() != Some(HEADER) {
        return Err(());
    }
    let mut rows = Vec::new();
    let (mut instagram, mut youtube) = (false, false);
    for row in lines {
        if row.is_empty() || (row.starts_with('#') && !row.starts_with("#HttpOnly_")) {
            continue;
        }
        let fields = cookie_fields(row)?;
        if !relevant_domain(fields[0]) {
            return Err(());
        }
        let domain = fields[0].strip_prefix('.').unwrap_or(fields[0]);
        if !fields[6].is_empty() {
            if (domain == "instagram.com" || domain.ends_with(".instagram.com"))
                && fields[5] == "sessionid"
            {
                instagram = true;
            }
            if (domain == "youtube.com"
                || domain.ends_with(".youtube.com")
                || domain == "google.com"
                || domain.ends_with(".google.com"))
                && matches!(fields[5], "SID" | "__Secure-1PSID" | "__Secure-3PSID")
            {
                youtube = true;
            }
        }
        rows.push(row);
    }
    if !instagram || !youtube {
        return Err(());
    }
    Ok(rows)
}

struct TempFile(PathBuf);
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn install_cookies(path: &Path, contents: &str) -> Result<(), ()> {
    let rows = validated_rows(contents)?;
    let mut combined = format!("{HEADER}\n");
    // Preserve validated unrelated providers, but discard all old IG/YT/Google rows.
    match read_limited(path, MAX_BODY) {
        Ok(existing) => {
            let existing = std::str::from_utf8(&existing).map_err(|_| ())?;
            for row in existing.lines() {
                if row.is_empty() || (row.starts_with('#') && !row.starts_with("#HttpOnly_")) {
                    continue;
                }
                let fields = cookie_fields(row)?;
                if !relevant_domain(fields[0]) {
                    combined.push_str(row);
                    combined.push('\n');
                }
            }
        }
        Err(()) if !path.exists() => {}
        Err(()) => return Err(()),
    }
    for row in rows {
        combined.push_str(row);
        combined.push('\n');
    }
    // Keep the merged file within the same bound for the next refresh too.
    if combined.len() > MAX_BODY {
        return Err(());
    }
    let directory = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let temporary = directory.join(format!(".cookies-refresh-{}.tmp", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).map_err(|_| ())?;
    // Only clean up files we actually created, never an unlikely collision.
    let temporary = TempFile(temporary);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|_| ())?;
    }
    file.write_all(combined.as_bytes()).map_err(|_| ())?;
    file.sync_all().map_err(|_| ())?;
    drop(file);
    std::fs::rename(&temporary.0, path).map_err(|_| ())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    const GOOD: &str = "# Netscape HTTP Cookie File\n#HttpOnly_.instagram.com\tTRUE\t/\tTRUE\t0\tsessionid\tfixture-ig\n.youtube.com\tTRUE\t/\tTRUE\t0\t__Secure-1PSID\tfixture-yt\n";

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("fetcher-cookie-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn config(&self, url: String) -> Config {
            let token_file = self.0.join("token");
            std::fs::write(&token_file, "fixture-token\n").unwrap();
            Config {
                url,
                token_file,
                cookies_file: self.0.join("cookies.txt"),
            }
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn force_with_state(
        state: &Mutex<RefreshState>,
        config: &Config,
        started: Instant,
        barrier: Option<&tokio::sync::Barrier>,
    ) -> bool {
        if let Some(barrier) = barrier {
            barrier.wait().await;
        }
        state
            .lock()
            .await
            .refresh_after_error(config, TIMEOUT, started)
            .await
    }

    #[tokio::test]
    async fn error_refresh_bypasses_success_ttl_but_has_its_own_cooldown() {
        let directory = Directory::new();
        let (url, initial) = server("200 OK", GOOD.into(), Duration::ZERO);
        let mut config = directory.config(url);
        let mut state = RefreshState::new();
        assert_eq!(state.refresh(&config, TIMEOUT).await, Some(true));
        initial.join().unwrap();
        let started = Instant::now();
        let (url, forced) = server("200 OK", GOOD.into(), Duration::ZERO);
        config.url = url;
        assert!(state.refresh_after_error(&config, TIMEOUT, started).await);
        forced.join().unwrap();
        // Every concurrent job that started with the old cookies may retry.
        assert!(state.refresh_after_error(&config, TIMEOUT, started).await);
        // A new permanent error after the successful forced update may not storm.
        assert!(
            !state
                .refresh_after_error(&config, TIMEOUT, Instant::now())
                .await
        );
    }

    #[tokio::test]
    async fn error_refresh_failure_backoff_retains_old_cookies() {
        let directory = Directory::new();
        let (url, server) = server("503 Unavailable", GOOD.into(), Duration::ZERO);
        let config = directory.config(url);
        std::fs::write(&config.cookies_file, GOOD).unwrap();
        let mut state = RefreshState::new();
        let started = Instant::now();
        assert!(!state.refresh_after_error(&config, TIMEOUT, started).await);
        server.join().unwrap();
        assert!(!state.refresh_after_error(&config, TIMEOUT, started).await);
        assert_eq!(state.refresh(&config, TIMEOUT).await, None);
        assert_eq!(std::fs::read_to_string(&config.cookies_file).unwrap(), GOOD);
    }

    #[tokio::test]
    async fn normal_refresh_failure_blocks_forced_request_and_cooldowns_expire() {
        let directory = Directory::new();
        let (url, failed) = server("503 Unavailable", GOOD.into(), Duration::ZERO);
        let mut config = directory.config(url);
        let mut state = RefreshState::new();
        assert_eq!(state.refresh(&config, TIMEOUT).await, Some(false));
        failed.join().unwrap();
        assert!(
            !state
                .refresh_after_error(&config, TIMEOUT, Instant::now())
                .await
        );
        assert!(
            state.last_error_attempt.is_none(),
            "failure backoff must skip even a forced request"
        );
        state.failure_until = Some(Instant::now() - Duration::from_secs(1));
        let (url, refreshed) = server("200 OK", GOOD.into(), Duration::ZERO);
        config.url = url;
        assert!(
            state
                .refresh_after_error(&config, TIMEOUT, Instant::now())
                .await
        );
        refreshed.join().unwrap();
        // Forced success cooldown expires even while regular TTL remains active.
        state.last_error_attempt = Some(Instant::now() - FAILURE_BACKOFF);
        let (url, refreshed) = server("200 OK", GOOD.into(), Duration::ZERO);
        config.url = url;
        assert!(
            state
                .refresh_after_error(&config, TIMEOUT, Instant::now())
                .await
        );
        refreshed.join().unwrap();
    }

    #[tokio::test]
    async fn simultaneous_errors_coalesce_and_each_can_retry() {
        let directory = Directory::new();
        let (url, server) = server("200 OK", GOOD.into(), Duration::from_millis(25));
        let config = directory.config(url);
        let state = Mutex::new(RefreshState::new());
        let started = Instant::now();
        let force = || async {
            state
                .lock()
                .await
                .refresh_after_error(&config, TIMEOUT, started)
                .await
        };
        assert_eq!(
            tokio::join!(force(), force(), force(), force()),
            (true, true, true, true)
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn real_subprocess_failure_refreshes_then_retries_same_operation_once() {
        let directory = Directory::new();
        let (url, server) = server("200 OK", GOOD.into(), Duration::ZERO);
        let config = directory.config(url);
        let state = Mutex::new(RefreshState::new());
        let count = directory.0.join("attempts");
        let output = run_output_with_refresh(
            || {
                let mut command = tokio::process::Command::new("sh");
                command
                    .arg("-c")
                    .arg("echo attempt >> \"$1\"; test -s \"$2\"")
                    .arg("fixture")
                    .arg(&count)
                    .arg(&config.cookies_file);
                command
            },
            true,
            None,
            |started| force_with_state(&state, &config, started, None),
        )
        .await
        .unwrap();
        assert!(output.status.success());
        assert_eq!(std::fs::read_to_string(count).unwrap().lines().count(), 2);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn streaming_subprocess_failure_refreshes_and_resumes_same_partial() {
        let directory = Directory::new();
        let (url, server) = server("200 OK", GOOD.into(), Duration::ZERO);
        let config = directory.config(url);
        let state = Mutex::new(RefreshState::new());
        let partial = directory.0.join("media.part");
        let attempts = std::cell::Cell::new(0);
        let bot = crate::Bot::new("fixture")
            .set_api_url(reqwest::Url::parse("http://127.0.0.1:9").unwrap());
        let status = run_with_refresh(true, || {
            attempts.set(attempts.get() + 1);
            let mut command = tokio::process::Command::new("sh");
            command.args(["-c", "printf '%s\\n' '[download] 1.0% fixture'; echo fixture-stderr >&2; if test -s \"$1\"; then test -s \"$2\"; else echo partial > \"$2\"; exit 1; fi", "fixture"])
                .arg(&config.cookies_file).arg(&partial);
            let bot = &bot;
            async move {
                let status = crate::download_progress_command(command, bot, crate::ChatId(1), crate::MessageId(1)).await?;
                Ok::<_, String>((status, status))
            }
        }, |started| force_with_state(&state, &config, started, None)).await.unwrap();
        assert!(status.success());
        assert_eq!(attempts.get(), 2);
        assert_eq!(std::fs::read_to_string(partial).unwrap(), "partial\n");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn subprocess_retry_exhaustion_is_bounded() {
        let attempts = std::cell::Cell::new(0);
        let refreshes = std::cell::Cell::new(0);
        let output = run_output_with_refresh(
            || {
                attempts.set(attempts.get() + 1);
                let mut c = tokio::process::Command::new("sh");
                c.args(["-c", "exit 1"]);
                c
            },
            true,
            None,
            |_| {
                refreshes.set(refreshes.get() + 1);
                std::future::ready(true)
            },
        )
        .await
        .unwrap();
        assert!(!output.status.success());
        assert_eq!((attempts.get(), refreshes.get()), (2, 1));
    }

    #[tokio::test]
    async fn success_spawn_timeout_x_and_failed_refresh_do_not_retry() {
        for (program, script, use_cookies, timeout, successful, refresh_count) in [
            ("sh", "exit 0", true, None, true, 0),
            ("/nonexistent/fetcher-yt-dlp", "", true, None, false, 0),
            (
                "sh",
                "exec sleep 1",
                true,
                Some(Duration::from_millis(10)),
                false,
                0,
            ),
            ("sh", "exit 1", false, None, false, 0),
            ("sh", "exit 1", true, None, false, 1),
        ] {
            let attempts = std::cell::Cell::new(0);
            let refreshes = std::cell::Cell::new(0);
            let result = run_output_with_refresh(
                || {
                    attempts.set(attempts.get() + 1);
                    let mut c = tokio::process::Command::new(program);
                    c.args(["-c", script]);
                    c
                },
                use_cookies,
                timeout,
                |_| {
                    refreshes.set(refreshes.get() + 1);
                    std::future::ready(false)
                },
            )
            .await;
            if timeout.is_some() {
                assert_eq!(
                    result.as_ref().unwrap_err().kind(),
                    std::io::ErrorKind::TimedOut
                );
            } else if program.starts_with("/nonexistent/") {
                assert_eq!(
                    result.as_ref().unwrap_err().kind(),
                    std::io::ErrorKind::NotFound
                );
            }
            assert_eq!(result.is_ok_and(|o| o.status.success()), successful);
            assert_eq!((attempts.get(), refreshes.get()), (1, refresh_count));
        }
    }

    #[tokio::test]
    async fn simultaneous_failed_subprocesses_each_retry_after_one_refresh() {
        let directory = Directory::new();
        let (url, server) = server("200 OK", GOOD.into(), Duration::from_millis(25));
        let config = directory.config(url);
        let state = Mutex::new(RefreshState::new());
        let barrier = tokio::sync::Barrier::new(4);
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let run = || async {
            run_output_with_refresh(
                || {
                    attempts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let mut command = tokio::process::Command::new("sh");
                    command
                        .args(["-c", "test -s \"$1\"", "fixture"])
                        .arg(&config.cookies_file);
                    command
                },
                true,
                None,
                |started| force_with_state(&state, &config, started, Some(&barrier)),
            )
            .await
            .unwrap()
            .status
            .success()
        };
        assert_eq!(
            tokio::join!(run(), run(), run(), run()),
            (true, true, true, true)
        );
        assert_eq!(attempts.load(std::sync::atomic::Ordering::Relaxed), 8);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn cancelled_error_refresh_keeps_both_cooldowns() {
        let directory = Directory::new();
        let (sent, received) = tokio::sync::oneshot::channel();
        let (url, server) = server_with_signal(
            "200 OK",
            GOOD.into(),
            Duration::from_millis(150),
            Some(sent),
        );
        let config = directory.config(url);
        let mut state = RefreshState::new();
        {
            let refresh = state.refresh_after_error(&config, TIMEOUT, Instant::now());
            tokio::pin!(refresh);
            // Cancel only after the fixture has read the request, not on a
            // scheduler-sensitive timeout that can close a half-written header.
            tokio::select! {
                biased;
                received = received => received.unwrap(),
                _ = &mut refresh => panic!("refresh completed before cancellation"),
            }
        }
        assert!(state.last_error_attempt.is_some());
        assert!(
            !state
                .refresh_after_error(&config, TIMEOUT, Instant::now())
                .await
        );
        assert_eq!(state.refresh(&config, TIMEOUT).await, None);
        assert!(!config.cookies_file.exists());
        server.join().unwrap();
    }

    #[tokio::test]
    #[ignore = "requires authorized running Firefox bridge and token file"]
    async fn live_cookie_refresh_after_yt_dlp_error() {
        let directory = Directory::new();
        let config = Config {
            url: std::env::var("FETCHER_LIVE_COOKIE_URL").expect("set live bridge URL"),
            token_file: std::env::var_os("FETCHER_LIVE_COOKIE_TOKEN_FILE")
                .map(PathBuf::from)
                .expect("set live bridge token file"),
            cookies_file: directory.0.join("cookies.txt"),
        };
        let state = Mutex::new(RefreshState::new());
        let attempts = std::cell::Cell::new(0);
        let output = run_output_with_refresh(
            || {
                attempts.set(attempts.get() + 1);
                let mut command = tokio::process::Command::new("sh");
                command
                    .args(["-c", "test -s \"$1\"", "fixture"])
                    .arg(&config.cookies_file);
                command
            },
            true,
            None,
            |started| force_with_state(&state, &config, started, None),
        )
        .await
        .expect("fixture subprocess should start");
        assert!(
            output.status.success(),
            "live error-triggered refresh/retry failed"
        );
        assert_eq!(attempts.get(), 2);
        let contents = std::fs::read_to_string(&config.cookies_file).unwrap();
        assert!(validated_rows(&contents).is_ok());
        println!("Live error-triggered refresh: validated isolated cookies, subprocess succeeded on attempt 2");
    }

    #[test]
    fn loopback_http_only() {
        for url in [
            "http://127.0.0.1:19121/refresh",
            "http://localhost:19121/refresh",
            "http://[::1]:19121/refresh",
            "http://127.12.34.56/refresh",
        ] {
            assert!(loopback_url(url).is_ok());
        }
        for url in [
            "https://127.0.0.1/refresh",
            "http://192.168.1.2/refresh",
            "http://example.com/refresh",
            "http://localhost.example.com/refresh",
            "http://127.0.0.1.evil.test/refresh",
            "http://user:secret@localhost/refresh",
            "http://localhost/refresh?token=secret",
            "http://localhost/refresh#fragment",
            "file:///refresh",
        ] {
            assert!(loopback_url(url).is_err());
        }
    }

    #[test]
    fn netscape_validation_requires_both_sessions_and_exact_domains() {
        assert!(validated_rows(GOOD).is_ok());
        assert!(validated_rows(&GOOD.replace(".youtube.com", ".accounts.google.com")).is_ok());
        for invalid in [
            GOOD.replace("# Netscape HTTP Cookie File", "not cookies"),
            GOOD.replace(".instagram.com", ".evilinstagram.com"),
            GOOD.replace(".youtube.com", ".youtube.com.evil.test"),
            GOOD.replace("fixture-ig", ""),
            GOOD.replace("fixture-yt", ""),
            GOOD.replace("sessionid", "csrftoken"),
            GOOD.replace("__Secure-1PSID", "PREF"),
            GOOD.replace("fixture-ig", "a\rInjected: value"),
            GOOD.replace("fixture-ig", "a\nInjected: value"),
            GOOD.replace("fixture-ig", "a\textra-column"),
            GOOD.replace("fixture-ig", "a;sessionid=injected"),
            GOOD.replace(".instagram.com\tTRUE", ".instagram.com\tFALSE"),
            GOOD.trim_end_matches('\n').to_string(),
            GOOD.replace("\tTRUE\t", "\tMAYBE\t"),
            GOOD.replace("\t0\t", "\tnot-time\t"),
            format!("{GOOD}.x.com\tTRUE\t/\tTRUE\t0\tauth_token\tfixture-x\n"),
        ] {
            assert!(validated_rows(&invalid).is_err());
        }
    }

    #[test]
    fn atomic_install_private_preserves_other_providers_and_old_file_on_rejection() {
        let directory = Directory::new();
        let path = directory.0.join("cookies.txt");
        let old = "# Netscape HTTP Cookie File\n.x.com\tTRUE\t/\tTRUE\t0\tauth_token\tfixture-x\n.instagram.com\tTRUE\t/\tTRUE\t0\tsessionid\told-ig\n";
        std::fs::write(&path, old).unwrap();
        assert!(install_cookies(&path, "invalid").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), old);
        let mut old_reader = std::fs::File::open(&path).unwrap();
        install_cookies(&path, GOOD).unwrap();
        // Existing readers retain the complete old inode, never a partial write.
        let mut old_view = String::new();
        old_reader.read_to_string(&mut old_view).unwrap();
        assert_eq!(old_view, old);
        let installed = std::fs::read_to_string(&path).unwrap();
        assert!(installed.contains("fixture-x"));
        assert!(installed.contains("fixture-ig"));
        assert!(!installed.contains("old-ig"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 1);
        let absent = directory.0.join("missing").join("cookies.txt");
        assert!(install_cookies(&absent, GOOD).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), installed);
        let blocked = directory.0.join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        assert!(install_cookies(&blocked, GOOD).is_err());
        assert!(blocked.is_dir());
        assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 2);
    }

    // A real isolated HTTP server: no bot, browser, cookies or public network.
    fn server(
        status: &str,
        body: String,
        delay: Duration,
    ) -> (String, std::thread::JoinHandle<()>) {
        server_with_signal(status, body, delay, None)
    }

    fn server_with_signal(
        status: &str,
        body: String,
        delay: Duration,
        received: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/refresh", listener.local_addr().unwrap());
        let status = status.to_string();
        let handle = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                assert!(request.len() < 16_384);
            }
            let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
            assert!(request.starts_with("post /refresh http/1.1\r\n"));
            assert!(request.contains("\r\nauthorization: bearer fixture-token\r\n"));
            if let Some(received) = received {
                let _ = received.send(());
            }
            std::thread::sleep(delay);
            let response = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nLocation: http://127.0.0.1:1/forbidden\r\nConnection: close\r\n\r\n{body}", body.len());
            let _ = socket.write_all(response.as_bytes());
        });
        (url, handle)
    }

    #[tokio::test]
    async fn real_http_success_ttl_and_failure_backoff() {
        let directory = Directory::new();
        let (url, server) = server("200 OK", GOOD.into(), Duration::ZERO);
        let config = directory.config(url);
        let mut state = RefreshState::new();
        assert_eq!(
            state.refresh(&config, Duration::from_secs(2)).await,
            Some(true)
        );
        server.join().unwrap();
        assert!(std::fs::read_to_string(&config.cookies_file)
            .unwrap()
            .contains("fixture-ig"));
        assert_eq!(state.refresh(&config, Duration::from_secs(2)).await, None);
        let remaining = state
            .next_attempt
            .unwrap()
            .saturating_duration_since(Instant::now());
        assert!(remaining > Duration::from_secs(595));
        state.next_attempt = None;
        assert_eq!(
            state.refresh(&config, Duration::from_secs(2)).await,
            Some(false)
        );
        let remaining = state
            .next_attempt
            .unwrap()
            .saturating_duration_since(Instant::now());
        assert!(remaining > Duration::from_secs(55) && remaining <= Duration::from_secs(60));
        assert_eq!(state.refresh(&config, Duration::from_secs(2)).await, None);
        assert!(std::fs::read_to_string(&config.cookies_file)
            .unwrap()
            .contains("fixture-ig"));
    }

    #[tokio::test]
    async fn http_invalid_redirect_status_oversize_and_timeout_preserve_last_good() {
        for (status, body, delay, timeout) in [
            (
                "200 OK",
                "invalid".into(),
                Duration::ZERO,
                Duration::from_secs(2),
            ),
            (
                "302 Found",
                GOOD.into(),
                Duration::ZERO,
                Duration::from_secs(2),
            ),
            (
                "401 Unauthorized",
                GOOD.into(),
                Duration::ZERO,
                Duration::from_secs(2),
            ),
            (
                "200 OK",
                "x".repeat(MAX_BODY + 1),
                Duration::ZERO,
                Duration::from_secs(2),
            ),
            (
                "200 OK",
                GOOD.into(),
                Duration::from_millis(150),
                Duration::from_millis(25),
            ),
        ] {
            let directory = Directory::new();
            let (url, server) = server(status, body, delay);
            let config = directory.config(url);
            std::fs::write(&config.cookies_file, "last-good").unwrap();
            let mut state = RefreshState::new();
            assert_eq!(state.refresh(&config, timeout).await, Some(false));
            assert_eq!(
                std::fs::read_to_string(&config.cookies_file).unwrap(),
                "last-good"
            );
            assert_eq!(state.refresh(&config, timeout).await, None);
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn concurrent_callers_share_one_refresh_and_localhost_is_pinned() {
        let directory = Directory::new();
        let (url, server) = server("200 OK", GOOD.into(), Duration::from_millis(25));
        let config = directory.config(url.replace("127.0.0.1", "localhost"));
        let state = Mutex::new(RefreshState::new());
        let attempt = || async {
            state
                .lock()
                .await
                .refresh(&config, Duration::from_secs(2))
                .await
        };
        let results = tokio::join!(attempt(), attempt(), attempt(), attempt());
        assert_eq!(results, (Some(true), None, None, None));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn malformed_token_and_url_preserve_file_without_a_request() {
        let directory = Directory::new();
        let mut config = directory.config("http://example.com/refresh".into());
        std::fs::write(&config.cookies_file, "last-good").unwrap();
        assert!(refresh_once(&config, TIMEOUT).await.is_err());
        config.url = "http://127.0.0.1:1/refresh".into();
        for token in ["", "fixture\r\nInjected: value", "fixture token"] {
            std::fs::write(&config.token_file, token).unwrap();
            assert!(refresh_once(&config, TIMEOUT).await.is_err());
        }
        assert_eq!(
            std::fs::read_to_string(&config.cookies_file).unwrap(),
            "last-good"
        );
    }

    #[tokio::test]
    async fn streaming_body_limit_and_body_read_timeout() {
        for slow in [false, true] {
            let directory = Directory::new();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let config =
                directory.config(format!("http://{}/refresh", listener.local_addr().unwrap()));
            std::fs::write(&config.cookies_file, "last-good").unwrap();
            let server = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                let mut byte = [0];
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    socket.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                socket.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").unwrap();
                if slow {
                    std::thread::sleep(Duration::from_millis(150));
                }
                let body = if slow {
                    GOOD.to_string()
                } else {
                    "x".repeat(MAX_BODY + 1)
                };
                let _ = write!(socket, "{:x}\r\n{body}\r\n0\r\n\r\n", body.len());
            });
            let timeout = if slow {
                Duration::from_millis(25)
            } else {
                Duration::from_secs(2)
            };
            assert!(refresh_once(&config, timeout).await.is_err());
            assert_eq!(
                std::fs::read_to_string(&config.cookies_file).unwrap(),
                "last-good"
            );
            server.join().unwrap();
        }
    }

    #[tokio::test]
    #[ignore = "requires authorized running Firefox bridge and token file"]
    async fn live_firefox_bridge_export() {
        let directory = Directory::new();
        let config = Config {
            url: std::env::var("FETCHER_LIVE_COOKIE_URL").expect("set live bridge URL"),
            token_file: std::env::var_os("FETCHER_LIVE_COOKIE_TOKEN_FILE")
                .map(PathBuf::from)
                .expect("set live bridge token file"),
            cookies_file: directory.0.join("cookies.txt"),
        };
        assert!(
            refresh_once(&config, TIMEOUT).await.is_ok(),
            "live export rejected"
        );
        let data = std::fs::read_to_string(&config.cookies_file).unwrap();
        assert!(validated_rows(&data).is_ok());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&config.cookies_file)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[tokio::test]
    async fn redirect_never_contacts_another_endpoint() {
        let directory = Directory::new();
        let destination = TcpListener::bind("127.0.0.1:0").unwrap();
        destination.set_nonblocking(true).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let config = directory.config(format!("http://{}/refresh", listener.local_addr().unwrap()));
        let target = destination.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut byte = [0];
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            write!(socket, "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{target}/refresh\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        assert!(refresh_once(&config, Duration::from_millis(200))
            .await
            .is_err());
        server.join().unwrap();
        assert_eq!(
            destination.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert!(!config.cookies_file.exists());
    }
}
