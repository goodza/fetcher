use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use regex::Regex;
use teloxide::prelude::*;
use teloxide::types::{
    InlineQueryResult, InlineQueryResultArticle, InlineQueryResultCachedVideo, InputFile,
    InputMessageContent, InputMessageContentText, MessageId, UserId,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::Instant;
use uuid::Uuid;

const DOWNLOAD_COOLDOWN: Duration = Duration::from_secs(60);
type DownloadLimiter = Arc<Mutex<HashMap<UserId, Instant>>>;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "info");
    }
    pretty_env_logger::init();
    log::info!("Starting fetcher bot...");
    check_cookies().await;

    let token = std::env::var("TG_TOKEN").expect("TG_TOKEN must be set");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("Failed to build HTTP client");
    let bot = Bot::with_client(token, client);
    let limiter: DownloadLimiter = Arc::new(Mutex::new(HashMap::new()));
    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint(handle_message))
        .branch(Update::filter_inline_query().endpoint(handle_inline_query));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![limiter])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}

#[derive(Clone, Copy)]
enum DownloadKind {
    InstagramReel,
    XVideo,
    YouTubeShort,
    YouTubeAudio,
}

impl DownloadKind {
    fn inline_title(self) -> &'static str {
        match self {
            Self::InstagramReel => "Instagram Reel",
            Self::XVideo => "X video",
            Self::YouTubeShort => "YouTube Short",
            Self::YouTubeAudio => "YouTube audio",
        }
    }

    fn is_inline_video(self) -> bool {
        matches!(
            self,
            Self::InstagramReel | Self::XVideo | Self::YouTubeShort
        )
    }

    fn log_kind(self) -> &'static str {
        match self {
            Self::InstagramReel => "instagram",
            Self::XVideo => "x",
            Self::YouTubeShort => "youtube_shorts",
            Self::YouTubeAudio => "youtube",
        }
    }

    fn downloading_message(self) -> &'static str {
        match self {
            Self::InstagramReel => "Downloading reel...",
            Self::XVideo => "Downloading X video...",
            Self::YouTubeShort => "Downloading short...",
            Self::YouTubeAudio => "Downloading audio...",
        }
    }

    fn title_fallback(self) -> &'static str {
        match self {
            Self::YouTubeAudio => "audio",
            _ => "video",
        }
    }

    fn output_extension(self) -> &'static str {
        match self {
            Self::YouTubeAudio => "mp3",
            _ => "mp4",
        }
    }

    fn format_args(self) -> &'static [&'static str] {
        match self {
            Self::YouTubeAudio => &["-x", "--audio-format", "mp3"],
            _ => &["-f", "mp4"],
        }
    }

    fn sending_message(self) -> &'static str {
        match self {
            Self::YouTubeAudio => "Sending audio...",
            _ => "Sending video...",
        }
    }
}

struct DownloadLink<'a> {
    kind: DownloadKind,
    url: &'a str,
}

fn find_download_link(text: &str) -> Option<DownloadLink<'_>> {
    let ig_re =
        Regex::new(r"https?://(?:www\.)?instagram\.com/(?:reel|reels)/[A-Za-z0-9_-]+").unwrap();
    let x_re = Regex::new(
        r"https?://(?:(?:www\.|mobile\.)?x\.com)/(?:[A-Za-z0-9_]+|i)/status/\d+(?:[/?#][^\s]*)?",
    )
    .unwrap();
    let yt_shorts_re =
        Regex::new(r"https?://(?:(?:www\.|m\.)?youtube\.com/shorts/[A-Za-z0-9_-]+)").unwrap();
    let yt_re = Regex::new(r"https?://(?:(?:www\.|m\.)?youtube\.com/watch\?[^\s]*v=[A-Za-z0-9_-]+|youtu\.be/[A-Za-z0-9_-]+)")
        .unwrap();

    if let Some(m) = ig_re.find(text) {
        Some(DownloadLink {
            kind: DownloadKind::InstagramReel,
            url: m.as_str(),
        })
    } else if let Some(m) = x_re.find(text) {
        Some(DownloadLink {
            kind: DownloadKind::XVideo,
            url: m.as_str(),
        })
    } else if let Some(m) = yt_shorts_re.find(text) {
        Some(DownloadLink {
            kind: DownloadKind::YouTubeShort,
            url: m.as_str(),
        })
    } else if let Some(m) = yt_re.find(text) {
        Some(DownloadLink {
            kind: DownloadKind::YouTubeAudio,
            url: m.as_str(),
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_x_video_status_link() {
        let link = find_download_link(
            "watch https://x.com/example_user/status/1800000000000000000?s=46&t=test",
        )
        .expect("x.com status link should be detected");

        assert!(matches!(link.kind, DownloadKind::XVideo));
        assert_eq!(
            link.url,
            "https://x.com/example_user/status/1800000000000000000?s=46&t=test"
        );
    }
}

fn check_download_limit(limiter: &DownloadLimiter, user_id: UserId) -> Result<(), u64> {
    let now = Instant::now();
    let mut downloads = limiter.lock().expect("download limiter lock poisoned");

    if let Some(last_download) = downloads.get(&user_id) {
        let elapsed = last_download.elapsed();
        if elapsed < DOWNLOAD_COOLDOWN {
            return Err((DOWNLOAD_COOLDOWN - elapsed).as_secs().max(1));
        }
    }

    downloads.insert(user_id, now);
    Ok(())
}

async fn handle_inline_query(
    bot: Bot,
    q: InlineQuery,
    limiter: DownloadLimiter,
) -> ResponseResult<()> {
    let results = if let Some(link) = find_download_link(&q.query) {
        if link.kind.is_inline_video() {
            match check_download_limit(&limiter, q.from.id) {
                Ok(()) => match prepare_inline_video(&bot, &q, &link).await {
                    Ok(result) => vec![InlineQueryResult::CachedVideo(result)],
                    Err(e) => vec![inline_article(
                        "error",
                        "Failed to prepare video",
                        format!("Failed to prepare inline video: {e}"),
                        "Try sending the link directly to the bot chat.",
                    )],
                },
                Err(wait_secs) => vec![inline_article(
                    "rate-limited",
                    "Wait before next download",
                    format!("Please wait {wait_secs}s before starting another download."),
                    "Limit: 1 download per minute per user.",
                )],
            }
        } else {
            vec![inline_article(
                "audio-not-supported",
                "Inline video mode needs a video link",
                link.url.to_string(),
                "YouTube watch links are still handled as audio in bot chat.",
            )]
        }
    } else {
        vec![inline_article(
            "help",
            "Paste an Instagram Reel, X video, or YouTube Short link",
            "Paste an Instagram Reel, X video, or YouTube Short link after the bot username.",
            "Example: @fetcher_bot https://www.instagram.com/reel/...",
        )]
    };

    bot.answer_inline_query(q.id, results)
        .cache_time(0)
        .is_personal(true)
        .await?;
    Ok(())
}

fn inline_article(
    id: &str,
    title: &str,
    message_text: impl Into<String>,
    description: &str,
) -> InlineQueryResult {
    InlineQueryResult::Article(
        InlineQueryResultArticle::new(
            id.to_string(),
            title,
            InputMessageContent::Text(InputMessageContentText::new(message_text)),
        )
        .description(description),
    )
}

async fn prepare_inline_video(
    bot: &Bot,
    q: &InlineQuery,
    link: &DownloadLink<'_>,
) -> Result<InlineQueryResultCachedVideo, String> {
    let tmp_path = std::env::temp_dir().join(format!("{}.mp4", Uuid::new_v4()));
    let title = fetch_title(link.url)
        .await
        .unwrap_or_else(|| "video".into());

    log_inline_download_link(link.kind.inline_title(), link.url, q).await;

    let status_msg = bot
        .send_message(q.from.id, "Preparing inline video...")
        .await
        .map_err(|e| format!("Cannot send progress message: {e}"))?;
    let progress_chat_id = status_msg.chat.id;

    let result = async {
        download_with_progress(
            link.url,
            &tmp_path,
            &["-f", "mp4"],
            bot,
            progress_chat_id,
            status_msg.id,
        )
        .await?;

        let metadata = tokio::fs::metadata(&tmp_path)
            .await
            .map_err(|e| format!("Cannot read downloaded file: {e}"))?;
        if metadata.len() > MAX_TG_SIZE {
            return Err(format!(
                "Downloaded video is too large for inline upload ({:.1}MB)",
                metadata.len() as f64 / 1024.0 / 1024.0
            ));
        }

        bot.edit_message_text(progress_chat_id, status_msg.id, "Uploading to Telegram...")
            .await
            .ok();

        let file = InputFile::file(&tmp_path).file_name(format!("{title}.mp4"));
        let uploaded = bot
            .send_video(q.from.id, file)
            .await
            .map_err(|e| format!("Telegram upload error: {e}"))?;
        let file_id = uploaded
            .video()
            .map(|video| video.file.id.clone())
            .ok_or_else(|| "Telegram response did not contain a video".to_string())?;

        bot.edit_message_text(progress_chat_id, status_msg.id, "Ready in inline results.")
            .await
            .ok();
        bot.delete_message(progress_chat_id, status_msg.id)
            .await
            .ok();

        Ok(
            InlineQueryResultCachedVideo::new("video-file", file_id, title)
                .description("Send downloaded video")
                .caption(link.url.to_string()),
        )
    }
    .await;

    if let Err(e) = &result {
        bot.edit_message_text(
            progress_chat_id,
            status_msg.id,
            format!("Inline video failed: {e}"),
        )
        .await
        .ok();
    }

    let _ = tokio::fs::remove_file(&tmp_path).await;
    result
}

async fn handle_message(bot: Bot, msg: Message, limiter: DownloadLimiter) -> ResponseResult<()> {
    let text = match msg.text() {
        Some(t) => t,
        None => return Ok(()),
    };

    if let Some(user) = msg.from.as_ref() {
        log::info!("Message from user id: {}", user.id);
    }

    let Some(link) = find_download_link(text) else {
        return Ok(());
    };

    if let Some(user) = msg.from.as_ref() {
        if let Err(wait_secs) = check_download_limit(&limiter, user.id) {
            bot.send_message(
                msg.chat.id,
                format!("Please wait {wait_secs}s before starting another download."),
            )
            .await?;
            return Ok(());
        }
    }

    let tmp_path = std::env::temp_dir().join(format!(
        "{}.{}",
        Uuid::new_v4(),
        link.kind.output_extension()
    ));

    log_download_link(link.kind.log_kind(), link.url, &msg).await;

    let status_msg = bot
        .send_message(msg.chat.id, link.kind.downloading_message())
        .await?;

    let title = fetch_title(link.url)
        .await
        .unwrap_or_else(|| link.kind.title_fallback().into());

    match download_with_progress(
        link.url,
        &tmp_path,
        link.kind.format_args(),
        &bot,
        msg.chat.id,
        status_msg.id,
    )
    .await
    {
        Ok(()) => {
            bot.edit_message_text(msg.chat.id, status_msg.id, link.kind.sending_message())
                .await
                .ok();

            let send_result = if link.kind.is_inline_video() {
                send_video(&bot, msg.chat.id, &tmp_path, &title).await
            } else {
                send_audio(&bot, msg.chat.id, &tmp_path, &title).await
            };

            if let Err(e) = send_result {
                let media_kind = if link.kind.is_inline_video() {
                    "video"
                } else {
                    "audio"
                };
                bot.edit_message_text(
                    msg.chat.id,
                    status_msg.id,
                    format!("Failed to send {media_kind}: {e}"),
                )
                .await?;
            } else {
                bot.delete_message(msg.chat.id, status_msg.id).await.ok();
            }
        }
        Err(e) => {
            bot.edit_message_text(msg.chat.id, status_msg.id, format!("Download failed: {e}"))
                .await?;
        }
    }

    let _ = tokio::fs::remove_file(&tmp_path).await;

    Ok(())
}

async fn log_download_link(kind: &str, url: &str, msg: &Message) {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let user_id = msg
        .from
        .as_ref()
        .map(|user| user.id.0.to_string())
        .unwrap_or_else(|| "unknown".into());
    let line = format!(
        "{ts}\t{kind}\tchat={}\tuser={}\t{url}\n",
        msg.chat.id, user_id
    );

    match tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("download_links.log")
        .await
    {
        Ok(mut file) => {
            if let Err(e) = file.write_all(line.as_bytes()).await {
                log::warn!("Failed to write download_links.log: {e}");
            }
        }
        Err(e) => log::warn!("Failed to open download_links.log: {e}"),
    }
}

async fn log_inline_download_link(kind: &str, url: &str, q: &InlineQuery) {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let line = format!(
        "{ts}\tinline_{kind}\tchat=inline\tuser={}\t{url}\n",
        q.from.id
    );

    match tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("download_links.log")
        .await
    {
        Ok(mut file) => {
            if let Err(e) = file.write_all(line.as_bytes()).await {
                log::warn!("Failed to write download_links.log: {e}");
            }
        }
        Err(e) => log::warn!("Failed to open download_links.log: {e}"),
    }
}

fn cookie_browser() -> &'static str {
    let brave_paths = if cfg!(target_os = "linux") {
        vec![dirs::config_dir().map(|d| d.join("BraveSoftware/Brave-Browser"))]
    } else if cfg!(target_os = "macos") {
        vec![dirs::data_dir().map(|d| d.join("BraveSoftware/Brave-Browser"))]
    } else {
        vec![dirs::data_local_dir().map(|d| d.join("BraveSoftware/Brave-Browser"))]
    };

    for path in brave_paths.into_iter().flatten() {
        if path.exists() {
            return "brave";
        }
    }

    "chrome"
}

fn add_cookie_args(cmd: &mut tokio::process::Command) {
    cmd.env_remove("NODE_APP_INSTANCE")
        .env_remove("NODE_CHANNEL_FD")
        .env_remove("NODE_CHANNEL_SERIALIZATION_MODE");

    if let Some(path) = cookie_file_path() {
        cmd.arg("--cookies").arg(path);
    } else {
        cmd.args(["--cookies-from-browser", cookie_browser()]);
    }
}

fn cookie_file_path() -> Option<PathBuf> {
    let cwd_path = PathBuf::from("cookies.txt");
    if cwd_path.exists() {
        return std::fs::canonicalize(cwd_path).ok();
    }

    let exe_path = std::env::current_exe().ok()?;
    let exe_dir = exe_path.parent()?;
    let exe_cookie_path = exe_dir.join("cookies.txt");
    if exe_cookie_path.exists() {
        std::fs::canonicalize(exe_cookie_path).ok()
    } else {
        None
    }
}

async fn check_cookies() {
    const CHECK_URL: &str = "https://www.youtube.com/watch?v=Sv5ZZB-M59Q";

    match cookie_file_path() {
        Some(path) => log::info!("Checking yt-dlp cookies from {}", path.display()),
        None => log::info!("Checking yt-dlp cookies from browser: {}", cookie_browser()),
    }

    let mut cmd = tokio::process::Command::new("yt-dlp");
    cmd.args([
        "--print",
        "title",
        "--no-download",
        "--js-runtimes",
        "node",
        "--remote-components",
        "ejs:github",
        "--verbose",
        CHECK_URL,
    ]);
    add_cookie_args(&mut cmd);

    match cmd.output().await {
        Ok(output) if output.status.success() => {
            let title = String::from_utf8_lossy(&output.stdout).trim().to_string();
            log::info!("yt-dlp cookie check OK: {title}");
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            for line in stderr.lines().filter(|line| {
                line.contains("JS runtimes")
                    || line.contains("jsc:")
                    || line.contains("cookies")
                    || line.contains("WARNING")
                    || line.contains("ERROR")
            }) {
                log::warn!("yt-dlp cookie check detail: {line}");
            }
            let details = stderr
                .lines()
                .last()
                .unwrap_or("yt-dlp exited with an error");
            log::error!("yt-dlp cookie check failed: {details}");
        }
        Err(e) => {
            log::error!("yt-dlp cookie check failed to run: {e}");
        }
    }
}

async fn download_with_progress(
    url: &str,
    output: &Path,
    format_args: &[&str],
    bot: &Bot,
    chat_id: ChatId,
    msg_id: MessageId,
) -> Result<(), String> {
    let mut cmd = tokio::process::Command::new("yt-dlp");
    cmd.args(format_args).args([
        "--newline",
        "--js-runtimes",
        "node",
        "--remote-components",
        "ejs:github",
        "-o",
        output.to_str().unwrap(),
        url,
    ]);
    add_cookie_args(&mut cmd);
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to run yt-dlp: {e}"))?;

    let extract_notified = Arc::new(AtomicBool::new(false));

    let stderr = child.stderr.take().unwrap();
    let stderr_bot = bot.clone();
    let stderr_extract_notified = Arc::clone(&extract_notified);
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            log::warn!("[yt-dlp stderr] {}", line);
            if line.contains("[ExtractAudio]")
                && !stderr_extract_notified.swap(true, Ordering::Relaxed)
            {
                stderr_bot
                    .edit_message_text(chat_id, msg_id, "Extracting audio...")
                    .await
                    .ok();
            }
        }
    });

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout).lines();

    let progress_re = Regex::new(r"\[download\]\s+(\d+\.?\d*%\s+.*)").unwrap();
    let extract_re = Regex::new(r"\[(?:ExtractAudio|ffmpeg)\]\s+(.+)$").unwrap();
    let mut last_update = Instant::now();
    let mut last_text = String::new();
    let update_interval = Duration::from_secs(3);

    while let Ok(Some(line)) = reader.next_line().await {
        log::info!("[yt-dlp] {}", line);
        if let Some(caps) = progress_re.captures(&line) {
            let progress = caps.get(1).unwrap().as_str().to_string();
            if last_update.elapsed() >= update_interval && progress != last_text {
                let display = format!("Downloading...\n{progress}");
                bot.edit_message_text(chat_id, msg_id, &display).await.ok();
                last_text = progress;
                last_update = Instant::now();
            }
        } else if let Some(caps) = extract_re.captures(&line) {
            let text = caps.get(1).unwrap().as_str().to_string();
            if line.contains("[ExtractAudio]") && !extract_notified.swap(true, Ordering::Relaxed) {
                bot.edit_message_text(chat_id, msg_id, "Extracting audio...")
                    .await
                    .ok();
                last_text = text;
                last_update = Instant::now();
            } else if last_update.elapsed() >= update_interval && text != last_text {
                let display = format!("Converting...\n{text}");
                bot.edit_message_text(chat_id, msg_id, &display).await.ok();
                last_text = text;
                last_update = Instant::now();
            }
        }
    }

    let status = child
        .wait()
        .await
        .map_err(|e| format!("yt-dlp error: {e}"))?;

    if !status.success() {
        return Err("yt-dlp exited with an error".into());
    }

    Ok(())
}

async fn fetch_title(url: &str) -> Option<String> {
    let mut cmd = tokio::process::Command::new("yt-dlp");
    cmd.args([
        "--print",
        "title",
        "--no-download",
        "--js-runtimes",
        "node",
        "--remote-components",
        "ejs:github",
        url,
    ]);
    add_cookie_args(&mut cmd);
    let output = cmd.output().await.ok()?;
    let title = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if title.is_empty() {
        None
    } else {
        Some(title)
    }
}

const MAX_TG_SIZE: u64 = 49 * 1024 * 1024;

async fn send_audio(bot: &Bot, chat_id: ChatId, path: &Path, title: &str) -> Result<(), String> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("Cannot read downloaded file: {e}"))?;

    if metadata.len() <= MAX_TG_SIZE {
        let file = InputFile::file(path).file_name(format!("{title}.mp3"));
        bot.send_audio(chat_id, file)
            .await
            .map_err(|e| format!("Telegram API error: {e}"))?;
        return Ok(());
    }

    let chunks = split_media(path, "mp3").await?;
    for (i, chunk) in chunks.iter().enumerate() {
        log::info!("Sending audio chunk {}/{}", i + 1, chunks.len());
        let label = if chunks.len() > 1 {
            format!("{title} (part {})", i + 1)
        } else {
            title.to_string()
        };
        let file = InputFile::file(chunk).file_name(format!("{label}.mp3"));
        bot.send_audio(chat_id, file)
            .await
            .map_err(|e| format!("Telegram API error on chunk {}: {e}", i + 1))?;
    }
    for chunk in &chunks {
        let _ = tokio::fs::remove_file(chunk).await;
    }

    Ok(())
}

async fn send_video(bot: &Bot, chat_id: ChatId, path: &Path, title: &str) -> Result<(), String> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("Cannot read downloaded file: {e}"))?;

    if metadata.len() <= MAX_TG_SIZE {
        let file = InputFile::file(path).file_name(format!("{title}.mp4"));
        bot.send_video(chat_id, file)
            .await
            .map_err(|e| format!("Telegram API error: {e}"))?;
        return Ok(());
    }

    let chunks = split_media(path, "mp4").await?;
    for (i, chunk) in chunks.iter().enumerate() {
        log::info!("Sending video chunk {}/{}", i + 1, chunks.len());
        let label = if chunks.len() > 1 {
            format!("{title} (part {})", i + 1)
        } else {
            title.to_string()
        };
        let file = InputFile::file(chunk).file_name(format!("{label}.mp4"));
        bot.send_video(chat_id, file)
            .await
            .map_err(|e| format!("Telegram API error on chunk {}: {e}", i + 1))?;
    }
    for chunk in &chunks {
        let _ = tokio::fs::remove_file(chunk).await;
    }

    Ok(())
}

async fn split_media(path: &Path, ext: &str) -> Result<Vec<PathBuf>, String> {
    let file_size = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("Cannot read file: {e}"))?
        .len();

    let num_chunks = (file_size / MAX_TG_SIZE) + 1;

    // Get total duration via ffprobe
    let probe = tokio::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
            path.to_str().unwrap(),
        ])
        .output()
        .await
        .map_err(|e| format!("Failed to run ffprobe: {e}"))?;

    let duration_str = String::from_utf8_lossy(&probe.stdout).trim().to_string();
    let total_duration: f64 = duration_str
        .parse()
        .map_err(|_| format!("Failed to parse duration: {duration_str}"))?;

    let chunk_duration = total_duration / num_chunks as f64;
    let dir = path.parent().unwrap();
    let stem = Uuid::new_v4();

    let pattern = dir.join(format!("{stem}_%03d.{ext}"));

    log::info!(
        "Splitting {:.1}MB file into {} chunks of ~{:.0}s each",
        file_size as f64 / 1024.0 / 1024.0,
        num_chunks,
        chunk_duration
    );

    let status = tokio::process::Command::new("ffmpeg")
        .args([
            "-i",
            path.to_str().unwrap(),
            "-f",
            "segment",
            "-segment_time",
            &format!("{chunk_duration:.2}"),
            "-c",
            "copy",
            "-reset_timestamps",
            "1",
            pattern.to_str().unwrap(),
        ])
        .status()
        .await
        .map_err(|e| format!("Failed to run ffmpeg: {e}"))?;

    if !status.success() {
        return Err("ffmpeg split failed".into());
    }

    // Collect generated chunk files
    let mut chunks = Vec::new();
    for i in 0u32.. {
        let chunk_path = dir.join(format!("{stem}_{:03}.{ext}", i));
        if chunk_path.exists() {
            chunks.push(chunk_path);
        } else {
            break;
        }
    }

    if chunks.is_empty() {
        return Err("No chunks produced by ffmpeg".into());
    }

    Ok(chunks)
}
