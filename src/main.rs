use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, LazyLock, Mutex,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use regex::Regex;
use teloxide::prelude::*;
use teloxide::types::{
    InlineKeyboardButton, InlineKeyboardMarkup, InlineQueryResult, InlineQueryResultArticle,
    InputFile, InputMessageContent, InputMessageContentText, MessageId,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};
use tokio::time::Instant;
use uuid::Uuid;

const DOWNLOAD_MENU_TTL: Duration = Duration::from_secs(30 * 60);
const MAX_DOWNLOAD_MENUS: usize = 1_024;

struct DownloadQueueState {
    semaphore: Arc<Semaphore>,
    waiting: AtomicUsize,
}

type DownloadQueue = Arc<DownloadQueueState>;

fn new_download_queue(max_concurrent: usize) -> DownloadQueue {
    Arc::new(DownloadQueueState {
        semaphore: Arc::new(Semaphore::new(max_concurrent)),
        waiting: AtomicUsize::new(0),
    })
}

fn parse_max_concurrent_downloads(value: &str) -> Result<usize, String> {
    match value.parse::<usize>() {
        Ok(0) => Err("must be greater than zero".into()),
        Ok(value) => Ok(value),
        Err(error) => Err(format!("must be a positive integer: {error}")),
    }
}

struct WaitingDownload {
    queue: DownloadQueue,
}

impl Drop for WaitingDownload {
    fn drop(&mut self) {
        self.queue.waiting.fetch_sub(1, Ordering::SeqCst);
    }
}

struct DownloadMenu {
    url: String,
    created_at: Instant,
}

type DownloadStore = Arc<Mutex<HashMap<String, DownloadMenu>>>;

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
    let max_concurrent_downloads =
        std::env::var("MAX_CONCURRENT_DOWNLOADS").expect("MAX_CONCURRENT_DOWNLOADS must be set");
    let max_concurrent_downloads = parse_max_concurrent_downloads(&max_concurrent_downloads)
        .expect("MAX_CONCURRENT_DOWNLOADS must be a positive integer");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("Failed to build HTTP client");
    let bot = Bot::with_client(token, client);
    let queue = new_download_queue(max_concurrent_downloads);
    let downloads: DownloadStore = Arc::new(Mutex::new(HashMap::new()));
    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint(handle_message))
        .branch(Update::filter_inline_query().endpoint(handle_inline_query))
        .branch(Update::filter_callback_query().endpoint(handle_callback_query));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![queue, downloads])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}

#[derive(Clone, Copy)]
enum DownloadKind {
    InstagramReel,
    InstagramProfile,
    XVideo,
    YouTubeShort,
    YouTubeVideo,
    YouTubeVideo480,
    YouTubeVideo720,
    YouTubeVideo1024,
    YouTubeVideo1440,
    YouTubeVideo2160,
    YouTubeAudio,
}

impl DownloadKind {
    fn is_inline_video(self) -> bool {
        matches!(
            self,
            Self::InstagramReel
                | Self::InstagramProfile
                | Self::XVideo
                | Self::YouTubeShort
                | Self::YouTubeVideo
                | Self::YouTubeVideo480
                | Self::YouTubeVideo720
                | Self::YouTubeVideo1024
                | Self::YouTubeVideo1440
                | Self::YouTubeVideo2160
        )
    }

    fn is_youtube(self) -> bool {
        matches!(
            self,
            Self::YouTubeShort
                | Self::YouTubeVideo
                | Self::YouTubeVideo480
                | Self::YouTubeVideo720
                | Self::YouTubeVideo1024
                | Self::YouTubeVideo1440
                | Self::YouTubeVideo2160
                | Self::YouTubeAudio
        )
    }

    fn is_youtube_video(self) -> bool {
        matches!(
            self,
            Self::YouTubeVideo
                | Self::YouTubeVideo480
                | Self::YouTubeVideo720
                | Self::YouTubeVideo1024
                | Self::YouTubeVideo1440
                | Self::YouTubeVideo2160
        )
    }

    fn log_kind(self) -> &'static str {
        match self {
            Self::InstagramReel => "instagram",
            Self::InstagramProfile => "instagram_profile",
            Self::XVideo => "x",
            Self::YouTubeShort => "youtube_shorts",
            Self::YouTubeVideo => "youtube_video",
            Self::YouTubeVideo480 => "youtube_video_480",
            Self::YouTubeVideo720 => "youtube_video_720",
            Self::YouTubeVideo1024 => "youtube_video_1024",
            Self::YouTubeVideo1440 => "youtube_video_1440",
            Self::YouTubeVideo2160 => "youtube_video_2160",
            Self::YouTubeAudio => "youtube",
        }
    }

    fn downloading_message(self) -> &'static str {
        match self {
            Self::InstagramReel => "Downloading reel...",
            Self::InstagramProfile => "Scrolling profile Reels...",
            Self::XVideo => "Downloading X video...",
            Self::YouTubeShort
            | Self::YouTubeVideo
            | Self::YouTubeVideo480
            | Self::YouTubeVideo720
            | Self::YouTubeVideo1024
            | Self::YouTubeVideo1440
            | Self::YouTubeVideo2160 => "Downloading video...",
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

    fn caption_field(self) -> Option<&'static str> {
        match self {
            Self::YouTubeAudio => Some("channel"),
            _ => None,
        }
    }

    fn format_args(self) -> &'static [&'static str] {
        match self {
            Self::XVideo => &[
                "-f",
                "best[ext=mp4][height<=720]/best[height<=720]/mp4",
                "--concat-playlist",
                "always",
            ],
            Self::YouTubeVideo | Self::YouTubeVideo1024 => &[
                "-f",
                "bestvideo[height<=1024][ext=mp4]+bestaudio[ext=m4a]/bestvideo[height<=1024]+bestaudio/best[height<=1024][ext=mp4]/best[height<=1024]/best",
                "--merge-output-format",
                "mp4",
            ],
            Self::YouTubeVideo720 => &[
                "-f",
                "bestvideo[height<=720][ext=mp4]+bestaudio[ext=m4a]/bestvideo[height<=720]+bestaudio/best[height<=720][ext=mp4]/best[height<=720]/best",
                "--merge-output-format",
                "mp4",
            ],
            Self::YouTubeVideo480 => &[
                "-f",
                "bestvideo[height<=480][ext=mp4]+bestaudio[ext=m4a]/bestvideo[height<=480]+bestaudio/best[height<=480][ext=mp4]/best[height<=480]/best",
                "--merge-output-format",
                "mp4",
            ],
            Self::YouTubeVideo1440 => &[
                "-f",
                "bestvideo[height<=1440][ext=mp4]+bestaudio[ext=m4a]/bestvideo[height<=1440]+bestaudio/best[height<=1440][ext=mp4]/best[height<=1440]/best",
                "--merge-output-format",
                "mp4",
            ],
            Self::YouTubeVideo2160 => &[
                "-f",
                "bestvideo[height<=2160][ext=mp4]+bestaudio[ext=m4a]/bestvideo[height<=2160]+bestaudio/best[height<=2160][ext=mp4]/best[height<=2160]/best",
                "--merge-output-format",
                "mp4",
            ],
            Self::YouTubeAudio => &["-x", "--audio-format", "mp3"],
            _ => &["-f", "b[ext=mp4]"],
        }
    }

    fn metadata_args(self) -> &'static [&'static str] {
        if self.is_youtube() {
            &["--embed-metadata", "--parse-metadata", "channel:artist"]
        } else {
            &[]
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

static IG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"https?://(?:www\.)?instagram\.com/(?:[A-Za-z0-9._]+/)?(?:reel|reels)/([A-Za-z0-9_-]+)/?",
    )
    .unwrap()
});
static IG_PROFILE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"https?://(?:www\.)?instagram\.com/([A-Za-z0-9._]+)/?").unwrap());
static X_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"https?://(?:(?:www\.|mobile\.)?x\.com)/(?:[A-Za-z0-9_]+|i)/status/\d+(?:[/?#][^\s]*)?",
    )
    .unwrap()
});
static YT_SHORTS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"https?://(?:(?:www\.|m\.)?youtube\.com/shorts/[A-Za-z0-9_-]+)").unwrap()
});
static YT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"https?://(?:(?:www\.|m\.)?youtube\.com/(?:watch\?[^\s]*v=[A-Za-z0-9_-]+|live/[A-Za-z0-9_-]+(?:[?#][^\s]*)?)|youtu\.be/[A-Za-z0-9_-]+)")
        .unwrap()
});
static HTTP_LINK_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://[^\s]+").unwrap());

fn contains_http_link(text: &str) -> bool {
    HTTP_LINK_RE.is_match(text)
}

fn find_download_link(text: &str) -> Option<DownloadLink<'_>> {
    if let Some(m) = IG_RE.find(text) {
        Some(DownloadLink {
            kind: DownloadKind::InstagramReel,
            url: m.as_str(),
        })
    } else if let Some(captures) = IG_PROFILE_RE.captures(text) {
        let username = captures.get(1)?.as_str();
        if matches!(
            username.to_ascii_lowercase().as_str(),
            "reel" | "reels" | "p" | "stories" | "explore"
        ) {
            return None;
        }
        Some(DownloadLink {
            kind: DownloadKind::InstagramProfile,
            url: captures.get(0)?.as_str(),
        })
    } else if let Some(m) = X_RE.find(text) {
        Some(DownloadLink {
            kind: DownloadKind::XVideo,
            url: m.as_str(),
        })
    } else if let Some(m) = YT_SHORTS_RE.find(text) {
        Some(DownloadLink {
            kind: DownloadKind::YouTubeShort,
            url: m.as_str(),
        })
    } else {
        YT_RE.find(text).map(|m| DownloadLink {
            kind: DownloadKind::YouTubeVideo,
            url: m.as_str(),
        })
    }
}

fn parse_youtube_download_callback(data: &str) -> Option<(DownloadKind, &str)> {
    data.strip_prefix("ytv480:")
        .map(|id| (DownloadKind::YouTubeVideo480, id))
        .or_else(|| {
            data.strip_prefix("ytv720:")
                .map(|id| (DownloadKind::YouTubeVideo720, id))
        })
        .or_else(|| {
            data.strip_prefix("ytv1024:")
                .map(|id| (DownloadKind::YouTubeVideo1024, id))
        })
        .or_else(|| {
            data.strip_prefix("ytv1440:")
                .map(|id| (DownloadKind::YouTubeVideo1440, id))
        })
        .or_else(|| {
            data.strip_prefix("ytv2160:")
                .map(|id| (DownloadKind::YouTubeVideo2160, id))
        })
        .or_else(|| {
            data.strip_prefix("ytv:")
                .map(|id| (DownloadKind::YouTubeVideo1024, id))
        })
        .or_else(|| {
            data.strip_prefix("yta:")
                .map(|id| (DownloadKind::YouTubeAudio, id))
        })
}

fn register_waiting_download(queue: &DownloadQueue) -> (usize, WaitingDownload) {
    let position = queue.waiting.fetch_add(1, Ordering::SeqCst) + 1;
    (
        position,
        WaitingDownload {
            queue: Arc::clone(queue),
        },
    )
}

async fn acquire_download_permit(
    queue: &DownloadQueue,
    bot: &Bot,
    chat_id: ChatId,
    status_msg_id: MessageId,
) -> OwnedSemaphorePermit {
    match Arc::clone(&queue.semaphore).try_acquire_owned() {
        Ok(permit) => permit,
        Err(TryAcquireError::NoPermits) => {
            let (position, waiting) = register_waiting_download(queue);
            let acquire = Arc::clone(&queue.semaphore).acquire_owned();
            tokio::pin!(acquire);

            let queued_text = format!("Queued… Position: {position}");
            let queued_message = async {
                bot.edit_message_text(chat_id, status_msg_id, queued_text)
                    .await
            };
            tokio::pin!(queued_message);

            // Poll the semaphore acquisition first so Tokio registers this waiter
            // before the Telegram status update can yield to a newer request.
            let permit = tokio::select! {
                biased;
                result = &mut acquire => result,
                _ = &mut queued_message => acquire.await,
            }
            .expect("download queue semaphore unexpectedly closed");

            drop(waiting);
            permit
        }
        Err(TryAcquireError::Closed) => {
            panic!("download queue semaphore unexpectedly closed")
        }
    }
}

fn prune_download_menus(downloads: &mut HashMap<String, DownloadMenu>, now: Instant) {
    downloads.retain(|_, menu| now.saturating_duration_since(menu.created_at) < DOWNLOAD_MENU_TTL);
}

fn insert_download_menu(
    downloads: &mut HashMap<String, DownloadMenu>,
    id: String,
    url: String,
    now: Instant,
) {
    prune_download_menus(downloads, now);

    while downloads.len() >= MAX_DOWNLOAD_MENUS {
        let Some(oldest_id) = downloads
            .iter()
            .min_by_key(|(_, menu)| menu.created_at)
            .map(|(id, _)| id.clone())
        else {
            break;
        };
        downloads.remove(&oldest_id);
    }

    downloads.insert(
        id,
        DownloadMenu {
            url,
            created_at: now,
        },
    );
}

fn take_download_url(
    downloads: &mut HashMap<String, DownloadMenu>,
    id: &str,
    now: Instant,
) -> Option<String> {
    prune_download_menus(downloads, now);
    downloads.remove(id).map(|menu| menu.url)
}

async fn handle_inline_query(
    bot: Bot,
    q: InlineQuery,
    _queue: DownloadQueue,
) -> ResponseResult<()> {
    let results = if let Some(link) = find_download_link(&q.query) {
        if link.kind.is_youtube() {
            vec![inline_article(
                "send-youtube-to-chat",
                "Open bot chat to choose quality",
                format!(
                    "Send this YouTube link to the bot chat to choose quality or audio:\n{}",
                    link.url
                ),
                "Inline queries answer instantly; downloads run in bot chat.",
            )]
        } else if link.kind.is_inline_video() {
            vec![inline_article(
                "send-video-to-chat",
                "Open bot chat to download",
                format!(
                    "Send this link to the bot chat to download it:\n{}",
                    link.url
                ),
                "Inline queries answer instantly; downloads run in bot chat.",
            )]
        } else {
            vec![inline_article(
                "audio-not-supported",
                "Open bot chat to download",
                link.url.to_string(),
                "Downloads run in bot chat.",
            )]
        }
    } else if contains_http_link(&q.query) {
        vec![inline_article(
            "unsupported-link",
            "Unsupported link",
            "This link is not supported.",
            "Supported: Instagram profiles or Reels, X videos, and YouTube videos or Shorts.",
        )]
    } else {
        vec![inline_article(
            "help",
            "Paste an Instagram profile or video link",
            "Paste an Instagram profile or Reel, X video, or YouTube link after the bot username.",
            "Example: @fetcher_bot https://www.instagram.com/example/",
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

async fn send_youtube_menu(
    bot: &Bot,
    chat_id: ChatId,
    url: &str,
    downloads: &DownloadStore,
) -> ResponseResult<()> {
    let id = Uuid::new_v4().to_string();
    {
        let mut downloads = downloads.lock().expect("download store lock poisoned");
        insert_download_menu(&mut downloads, id.clone(), url.to_string(), Instant::now());
    }

    let keyboard = InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("480p", format!("ytv480:{id}")),
            InlineKeyboardButton::callback("720p", format!("ytv720:{id}")),
        ],
        vec![
            InlineKeyboardButton::callback("1024p", format!("ytv1024:{id}")),
            InlineKeyboardButton::callback("4K", format!("ytv2160:{id}")),
        ],
        vec![InlineKeyboardButton::callback("Audio", format!("yta:{id}"))],
    ]);

    let result = bot
        .send_message(chat_id, "Choose quality or audio:")
        .reply_markup(keyboard)
        .await;

    if result.is_err() {
        let mut downloads = downloads.lock().expect("download store lock poisoned");
        downloads.remove(&id);
    }
    result?;

    Ok(())
}

async fn download_and_send_media(
    bot: &Bot,
    chat_id: ChatId,
    status_msg_id: MessageId,
    kind: DownloadKind,
    url: &str,
) -> Result<(), String> {
    if matches!(kind, DownloadKind::InstagramProfile) {
        let result = download_instagram_profile_and_send(bot, chat_id, status_msg_id, url).await;
        if result.is_ok() {
            bot.delete_message(chat_id, status_msg_id).await.ok();
        }
        return result;
    }

    let tmp_path =
        std::env::temp_dir().join(format!("{}.{}", Uuid::new_v4(), kind.output_extension()));

    let result = async {
        let title = fetch_metadata_field(url, "title")
            .await
            .unwrap_or_else(|| kind.title_fallback().into());
        let channel = if let Some(field) = kind.caption_field() {
            fetch_metadata_field(url, field).await
        } else {
            None
        };

        download_with_progress(
            url,
            &tmp_path,
            kind.format_args(),
            kind.metadata_args(),
            bot,
            chat_id,
            status_msg_id,
        )
        .await?;

        bot.edit_message_text(chat_id, status_msg_id, kind.sending_message())
            .await
            .ok();

        if kind.is_inline_video() {
            send_video(bot, chat_id, &tmp_path, &title, kind).await
        } else {
            send_audio(bot, chat_id, &tmp_path, &title, channel.as_deref()).await
        }
    }
    .await;

    let _ = tokio::fs::remove_file(&tmp_path).await;

    if result.is_ok() {
        bot.delete_message(chat_id, status_msg_id).await.ok();
    }

    result
}

fn instagram_profile_username(url: &str) -> Option<&str> {
    IG_PROFILE_RE
        .captures(url)
        .and_then(|captures| captures.get(1))
        .map(|username| username.as_str())
}

async fn webdriver_json(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    body: Option<serde_json::Value>,
    operation: &str,
) -> Result<serde_json::Value, String> {
    let mut request = client.request(method, url);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request
        .send()
        .await
        .map_err(|e| format!("Browser {operation} failed: {e}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("Cannot read browser {operation} response: {e}"))?;

    if !status.is_success() {
        return Err(format!(
            "Browser {operation} returned HTTP {status}: {body}"
        ));
    }

    let response: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| format!("Browser {operation} returned invalid JSON: {e}"))?;
    if let Some(error) = response
        .pointer("/value/error")
        .and_then(serde_json::Value::as_str)
    {
        let message = response
            .pointer("/value/message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(error);
        return Err(format!("Browser {operation} failed: {message}"));
    }
    Ok(response)
}

fn instagram_webdriver_cookies() -> Vec<serde_json::Value> {
    let Some(cookie_file) = cookie_file_path() else {
        return Vec::new();
    };
    let Ok(contents) = std::fs::read_to_string(cookie_file) else {
        return Vec::new();
    };

    contents
        .lines()
        .filter_map(|raw_line| {
            let line = raw_line.strip_prefix("#HttpOnly_").unwrap_or(raw_line);
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let fields: Vec<_> = line.split('\t').collect();
            if fields.len() < 7 || !fields[0].ends_with("instagram.com") {
                return None;
            }

            let mut cookie = serde_json::json!({
                "name": fields[5],
                "value": fields[6],
                "path": fields[2],
                "domain": fields[0],
                "secure": fields[3].eq_ignore_ascii_case("TRUE"),
            });
            if let Ok(expiry) = fields[4].parse::<u64>() {
                if expiry > 0 {
                    cookie["expiry"] = serde_json::json!(expiry);
                }
            }
            Some(cookie)
        })
        .collect()
}

fn instagram_reel_shortcode(url: &str) -> Option<&str> {
    IG_RE
        .captures(url)
        .and_then(|captures| captures.get(1))
        .map(|shortcode| shortcode.as_str())
}

async fn scrape_instagram_reel_urls(
    client: &reqwest::Client,
    webdriver_url: &str,
    session_id: &str,
    username: &str,
) -> Result<Vec<String>, String> {
    let session_url = format!("{webdriver_url}/session/{session_id}");

    webdriver_json(
        client,
        reqwest::Method::POST,
        &format!("{session_url}/url"),
        Some(serde_json::json!({"url": "https://www.instagram.com/"})),
        "opening Instagram",
    )
    .await?;

    for cookie in instagram_webdriver_cookies() {
        if let Err(error) = webdriver_json(
            client,
            reqwest::Method::POST,
            &format!("{session_url}/cookie"),
            Some(serde_json::json!({"cookie": cookie})),
            "loading an Instagram cookie",
        )
        .await
        {
            log::warn!("{error}");
        }
    }

    let reels_page = format!("https://www.instagram.com/{username}/reels/");
    webdriver_json(
        client,
        reqwest::Method::POST,
        &format!("{session_url}/url"),
        Some(serde_json::json!({"url": reels_page})),
        "opening the Instagram Reels page",
    )
    .await?;

    let execute_url = format!("{session_url}/execute/sync");
    let mut seen_shortcodes = HashSet::new();
    let mut urls = Vec::new();
    let mut last_height = 0_u64;
    let mut unchanged_rounds = 0_u8;
    let mut reached_end = false;

    // Instagram virtualizes long grids, so collect links before every scroll
    // instead of parsing only the final DOM.
    for _ in 0..600 {
        let response = webdriver_json(
            client,
            reqwest::Method::POST,
            &execute_url,
            Some(serde_json::json!({
                "script": r#"
                    const urls = Array.from(document.querySelectorAll('a[href]'))
                        .map(anchor => anchor.href)
                        .filter(url => /\/(?:[A-Za-z0-9._]+\/)?reels?\/[A-Za-z0-9_-]+\/?/.test(url));
                    const height = Math.max(
                        document.body?.scrollHeight || 0,
                        document.documentElement?.scrollHeight || 0
                    );
                    const text = document.body?.innerText || '';
                    const pageUrl = location.href;
                    window.scrollTo(0, height);
                    return {urls, height, text: text.slice(0, 4000), pageUrl};
                "#,
                "args": [],
            })),
            "reading and scrolling the Instagram Reels page",
        )
        .await?;
        let value = response
            .get("value")
            .ok_or_else(|| "Browser returned no page data".to_string())?;
        let page_url = value
            .get("pageUrl")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if page_url.contains("/accounts/login") {
            return Err(
                "Instagram opened the login page; refresh the Instagram cookies.txt session".into(),
            );
        }

        let before = urls.len();
        if let Some(found_urls) = value.get("urls").and_then(serde_json::Value::as_array) {
            for found_url in found_urls.iter().filter_map(serde_json::Value::as_str) {
                let Some(shortcode) = instagram_reel_shortcode(found_url) else {
                    continue;
                };
                if seen_shortcodes.insert(shortcode.to_string()) {
                    urls.push(format!("https://www.instagram.com/reel/{shortcode}/"));
                }
            }
        }

        let height = value
            .get("height")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default();
        if urls.len() == before && height == last_height {
            unchanged_rounds += 1;
        } else {
            unchanged_rounds = 0;
        }
        if unchanged_rounds >= 6 {
            reached_end = true;
            break;
        }
        last_height = height;
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }

    if !reached_end {
        Err("Instagram scrolling did not reach a stable bottom; refusing to return a possibly incomplete Reel list".into())
    } else if urls.is_empty() {
        Err("No Reel links were found while scrolling this Instagram profile; the profile may be private, unavailable, or require fresh cookies".into())
    } else {
        Ok(urls)
    }
}

async fn gather_instagram_reel_urls(username: &str) -> Result<Vec<String>, String> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| format!("Cannot reserve a ChromeDriver port: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("Cannot read the ChromeDriver port: {e}"))?
        .port();
    drop(listener);

    let chromedriver = std::env::var("CHROMEDRIVER").unwrap_or_else(|_| "chromedriver".to_string());
    let mut driver = tokio::process::Command::new(chromedriver)
        .arg(format!("--port={port}"))
        .arg("--allowed-ips=127.0.0.1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Cannot start ChromeDriver: {e}"))?;

    let webdriver_url = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| format!("Cannot create browser-control client: {e}"))?;

    let mut ready = false;
    for _ in 0..50 {
        if client
            .get(format!("{webdriver_url}/status"))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
        {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if !ready {
        let _ = driver.kill().await;
        return Err("ChromeDriver did not become ready".into());
    }

    let mut chrome_args = vec![
        "--headless=new".to_string(),
        "--no-sandbox".to_string(),
        "--disable-dev-shm-usage".to_string(),
        "--disable-blink-features=AutomationControlled".to_string(),
        "--window-size=1280,1000".to_string(),
        "--lang=en-US".to_string(),
    ];
    if let Ok(extra_args) = std::env::var("INSTAGRAM_CHROME_ARGS") {
        chrome_args.extend(extra_args.split_whitespace().map(str::to_string));
    }
    let mut chrome_options = serde_json::json!({"args": chrome_args});
    if let Ok(binary) = std::env::var("CHROME_BINARY") {
        chrome_options["binary"] = serde_json::Value::String(binary);
    }

    let session = webdriver_json(
        &client,
        reqwest::Method::POST,
        &format!("{webdriver_url}/session"),
        Some(serde_json::json!({
            "capabilities": {
                "alwaysMatch": {
                    "browserName": "chrome",
                    "pageLoadStrategy": "eager",
                    "goog:chromeOptions": chrome_options,
                }
            }
        })),
        "session creation",
    )
    .await;
    let session_id = match session {
        Ok(session) => session
            .pointer("/value/sessionId")
            .or_else(|| session.get("sessionId"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "ChromeDriver returned no session ID".to_string()),
        Err(error) => Err(error),
    };
    let session_id = match session_id {
        Ok(session_id) => session_id,
        Err(error) => {
            let _ = driver.kill().await;
            return Err(error);
        }
    };

    let result = scrape_instagram_reel_urls(&client, &webdriver_url, &session_id, username).await;
    let _ = webdriver_json(
        &client,
        reqwest::Method::DELETE,
        &format!("{webdriver_url}/session/{session_id}"),
        None,
        "session cleanup",
    )
    .await;
    let _ = driver.kill().await;
    result
}

async fn concatenate_videos(inputs: &[PathBuf], output: &Path) -> Result<(), String> {
    if inputs.len() == 1 {
        tokio::fs::copy(&inputs[0], output)
            .await
            .map_err(|e| format!("Cannot copy the downloaded Reel: {e}"))?;
        return Ok(());
    }

    let list_path = output.with_extension("txt");
    let list = inputs
        .iter()
        .map(|path| format!("file '{}'\n", path.display()))
        .collect::<String>();
    tokio::fs::write(&list_path, list)
        .await
        .map_err(|e| format!("Cannot create ffmpeg concat list: {e}"))?;

    let result = tokio::process::Command::new("ffmpeg")
        .args(["-y", "-f", "concat", "-safe", "0", "-i"])
        .arg(&list_path)
        .args(["-c", "copy", "-movflags", "+faststart"])
        .arg(output)
        .output()
        .await
        .map_err(|e| format!("Failed to run ffmpeg: {e}"));
    let _ = tokio::fs::remove_file(&list_path).await;
    let result = result?;

    if !result.status.success() {
        let details = String::from_utf8_lossy(&result.stderr)
            .lines()
            .last()
            .unwrap_or("ffmpeg exited with an error")
            .to_string();
        return Err(format!("ffmpeg concat failed: {details}"));
    }

    Ok(())
}

async fn download_instagram_profile_and_send(
    bot: &Bot,
    chat_id: ChatId,
    status_msg_id: MessageId,
    url: &str,
) -> Result<(), String> {
    let username = instagram_profile_username(url)
        .ok_or_else(|| "Cannot determine the Instagram username".to_string())?;
    let work_dir = std::env::temp_dir().join(format!("instagram-profile-{}", Uuid::new_v4()));
    tokio::fs::create_dir(&work_dir)
        .await
        .map_err(|e| format!("Cannot create temporary directory: {e}"))?;

    let result = async {
        bot.edit_message_text(chat_id, status_msg_id, "Scrolling profile Reels...")
            .await
            .ok();
        let reel_urls = gather_instagram_reel_urls(username).await?;
        let mut videos = Vec::with_capacity(reel_urls.len());

        for (index, reel_url) in reel_urls.iter().enumerate() {
            bot.edit_message_text(
                chat_id,
                status_msg_id,
                format!("Downloading Reel {}/{}...", index + 1, reel_urls.len()),
            )
            .await
            .ok();
            let path = work_dir.join(format!("{index:05}.mp4"));
            download_with_progress(
                reel_url,
                &path,
                DownloadKind::InstagramReel.format_args(),
                DownloadKind::InstagramReel.metadata_args(),
                bot,
                chat_id,
                status_msg_id,
            )
            .await?;
            videos.push(path);
        }

        bot.edit_message_text(
            chat_id,
            status_msg_id,
            format!("Concatenating {} Reels...", videos.len()),
        )
        .await
        .ok();
        let output = work_dir.join("reels.mp4");
        concatenate_videos(&videos, &output).await?;

        bot.edit_message_text(chat_id, status_msg_id, "Sending video...")
            .await
            .ok();
        send_video(
            bot,
            chat_id,
            &output,
            &format!("{username} reels"),
            DownloadKind::InstagramProfile,
        )
        .await
    }
    .await;

    let _ = tokio::fs::remove_dir_all(&work_dir).await;
    result
}

async fn handle_callback_query(
    bot: Bot,
    q: CallbackQuery,
    queue: DownloadQueue,
    downloads: DownloadStore,
) -> ResponseResult<()> {
    let Some(data) = q.data.as_deref() else {
        return Ok(());
    };
    let Some((kind, id)) = parse_youtube_download_callback(data) else {
        return Ok(());
    };

    let Some(message) = q.regular_message() else {
        bot.answer_callback_query(q.id)
            .text("Cannot access this menu message. Send the YouTube link again.")
            .show_alert(true)
            .await?;
        return Ok(());
    };
    let chat_id = message.chat.id;
    let status_msg_id = message.id;

    let url = {
        let mut downloads = downloads.lock().expect("download store lock poisoned");
        take_download_url(&mut downloads, id, Instant::now())
    };
    let Some(url) = url else {
        bot.answer_callback_query(q.id)
            .text("This download menu expired. Send the YouTube link again.")
            .show_alert(true)
            .await?;
        return Ok(());
    };

    bot.answer_callback_query(q.id.clone()).await?;

    let result = {
        let _permit = acquire_download_permit(&queue, &bot, chat_id, status_msg_id).await;

        log_callback_download_link(kind.log_kind(), &url, &q).await;

        bot.edit_message_text(chat_id, status_msg_id, kind.downloading_message())
            .await
            .ok();

        download_and_send_media(&bot, chat_id, status_msg_id, kind, &url).await
    };

    if let Err(e) = result {
        bot.edit_message_text(chat_id, status_msg_id, format!("Download failed: {e}"))
            .await?;
    }

    Ok(())
}

async fn handle_message(
    bot: Bot,
    msg: Message,
    queue: DownloadQueue,
    downloads: DownloadStore,
) -> ResponseResult<()> {
    let text = match msg.text() {
        Some(t) => t,
        None => return Ok(()),
    };

    if let Some(user) = msg.from.as_ref() {
        log::info!("Message from user id: {}", user.id);
    }

    let Some(link) = find_download_link(text) else {
        if contains_http_link(text) {
            bot.send_message(
                msg.chat.id,
                "Unsupported link. Supported: Instagram profiles or Reels, X videos, and YouTube videos or Shorts.",
            )
            .await?;
        }
        return Ok(());
    };

    if link.kind.is_youtube() {
        send_youtube_menu(&bot, msg.chat.id, link.url, &downloads).await?;
        return Ok(());
    }

    let status_msg = bot
        .send_message(msg.chat.id, link.kind.downloading_message())
        .await?;

    let result = {
        let _permit = acquire_download_permit(&queue, &bot, msg.chat.id, status_msg.id).await;

        log_download_link(link.kind.log_kind(), link.url, &msg).await;

        bot.edit_message_text(msg.chat.id, status_msg.id, link.kind.downloading_message())
            .await
            .ok();

        download_and_send_media(&bot, msg.chat.id, status_msg.id, link.kind, link.url).await
    };

    if let Err(e) = result {
        bot.edit_message_text(msg.chat.id, status_msg.id, format!("Download failed: {e}"))
            .await?;
    }

    Ok(())
}

async fn log_download_link(kind: &str, url: &str, msg: &Message) {
    let user_id = msg
        .from
        .as_ref()
        .map(|user| user.id.0.to_string())
        .unwrap_or_else(|| "unknown".into());
    append_download_log(kind, &user_id, url).await;
}

async fn log_callback_download_link(kind: &str, url: &str, q: &CallbackQuery) {
    append_download_log(&format!("callback_{kind}"), &q.from.id.to_string(), url).await;
}

async fn append_download_log(kind: &str, user_id: &str, url: &str) {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let line = format!("{ts}\t{kind}\tuser={user_id}\t{url}\n");

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

fn should_use_cookies(url: &str) -> bool {
    !X_RE.is_match(url)
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
    metadata_args: &[&str],
    bot: &Bot,
    chat_id: ChatId,
    msg_id: MessageId,
) -> Result<(), String> {
    let mut cmd = tokio::process::Command::new("yt-dlp");
    cmd.args(format_args).args(metadata_args).args([
        "--newline",
        "--js-runtimes",
        "node",
        "--remote-components",
        "ejs:github",
        "-o",
        output.to_str().unwrap(),
        url,
    ]);
    // X guest-token extraction works without authentication. Stale browser
    // cookies can instead make its GraphQL endpoint reject the request.
    if should_use_cookies(url) {
        add_cookie_args(&mut cmd);
    }
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

async fn fetch_metadata_field(url: &str, field: &str) -> Option<String> {
    let mut cmd = tokio::process::Command::new("yt-dlp");
    cmd.args([
        "--print",
        field,
        "--no-download",
        "--js-runtimes",
        "node",
        "--remote-components",
        "ejs:github",
        url,
    ]);
    if should_use_cookies(url) {
        add_cookie_args(&mut cmd);
    }
    let output = cmd.output().await.ok()?;
    let title = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if title.is_empty() {
        None
    } else {
        Some(title)
    }
}

const MAX_TG_SIZE: u64 = 49 * 1024 * 1024;

fn is_request_entity_too_large(error: &teloxide::RequestError) -> bool {
    matches!(
        error,
        teloxide::RequestError::Api(teloxide::ApiError::RequestEntityTooLarge)
    )
}

async fn send_video_with_document_fallback(
    bot: &Bot,
    chat_id: ChatId,
    path: &Path,
    file_name: String,
) -> Result<(), teloxide::RequestError> {
    let video = InputFile::file(path).file_name(file_name.clone());
    match bot.send_video(chat_id, video).await {
        Ok(_) => Ok(()),
        Err(error) if is_request_entity_too_large(&error) => {
            log::info!("Telegram rejected video upload as too large; retrying as document");
            let document = InputFile::file(path).file_name(file_name);
            bot.send_document(chat_id, document).await.map(|_| ())
        }
        Err(error) => Err(error),
    }
}

async fn send_audio(
    bot: &Bot,
    chat_id: ChatId,
    path: &Path,
    title: &str,
    channel: Option<&str>,
) -> Result<(), String> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("Cannot read downloaded file: {e}"))?;

    if metadata.len() <= MAX_TG_SIZE {
        let file = InputFile::file(path).file_name(format!("{title}.mp3"));
        let mut request = bot.send_audio(chat_id, file).title(title);
        if let Some(channel) = channel {
            request = request.performer(channel).caption(channel);
        }
        request
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
        let mut request = bot.send_audio(chat_id, file).title(label);
        if let Some(channel) = channel {
            request = request.performer(channel).caption(channel);
        }
        request
            .await
            .map_err(|e| format!("Telegram API error on chunk {}: {e}", i + 1))?;
    }
    for chunk in &chunks {
        let _ = tokio::fs::remove_file(chunk).await;
    }

    Ok(())
}

async fn send_video(
    bot: &Bot,
    chat_id: ChatId,
    path: &Path,
    title: &str,
    kind: DownloadKind,
) -> Result<(), String> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("Cannot read downloaded file: {e}"))?;

    if metadata.len() <= MAX_TG_SIZE {
        send_video_with_document_fallback(bot, chat_id, path, format!("{title}.mp4"))
            .await
            .map_err(|e| format!("Telegram API error sending file: {e}"))?;
        return Ok(());
    }

    if kind.is_youtube_video() {
        log::info!(
            "Sending oversized YouTube video as document ({:.1}MB)",
            metadata.len() as f64 / 1024.0 / 1024.0
        );
        let file = InputFile::file(path).file_name(format!("{title}.mp4"));
        match bot.send_document(chat_id, file).await {
            Ok(_) => return Ok(()),
            Err(error) if is_request_entity_too_large(&error) => {
                log::info!(
                    "Telegram rejected document upload as too large; splitting into smaller parts"
                );
            }
            Err(error) => return Err(format!("Telegram API error sending file: {error}")),
        }
    }

    let chunks = split_media(path, "mp4").await?;
    for (i, chunk) in chunks.iter().enumerate() {
        log::info!("Sending video chunk {}/{}", i + 1, chunks.len());
        let label = if chunks.len() > 1 {
            format!("{title} (part {})", i + 1)
        } else {
            title.to_string()
        };
        send_video_with_document_fallback(bot, chat_id, chunk, format!("{label}.mp4"))
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

    let num_chunks = chunk_count(file_size, MAX_TG_SIZE);

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

    if let Err(e) = validate_chunk_sizes(&chunks).await {
        for chunk in &chunks {
            let _ = tokio::fs::remove_file(chunk).await;
        }
        return Err(e);
    }

    Ok(chunks)
}

fn chunk_count(file_size: u64, max_size: u64) -> u64 {
    file_size.div_ceil(max_size).max(1)
}

async fn validate_chunk_sizes(chunks: &[PathBuf]) -> Result<(), String> {
    for chunk in chunks {
        let size = tokio::fs::metadata(chunk)
            .await
            .map_err(|e| format!("Cannot read split chunk {}: {e}", chunk.display()))?
            .len();

        if size > MAX_TG_SIZE {
            return Err(format!(
                "Split chunk {} is too large ({:.1}MB)",
                chunk.display(),
                size as f64 / 1024.0 / 1024.0
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_instagram_profile_link() {
        let link = find_download_link("https://www.instagram.com/kolka_elf")
            .expect("Instagram profile link should be detected");

        assert!(matches!(link.kind, DownloadKind::InstagramProfile));
        assert_eq!(link.url, "https://www.instagram.com/kolka_elf");
        assert_eq!(instagram_profile_username(link.url), Some("kolka_elf"));
    }

    #[test]
    fn finds_username_prefixed_instagram_reel_link() {
        let link = find_download_link("https://www.instagram.com/kolka_elf/reel/DbHN_ovssLw/")
            .expect("username-prefixed Instagram reel link should be detected");

        assert!(matches!(link.kind, DownloadKind::InstagramReel));
        assert_eq!(
            link.url,
            "https://www.instagram.com/kolka_elf/reel/DbHN_ovssLw/"
        );
        assert_eq!(instagram_reel_shortcode(link.url), Some("DbHN_ovssLw"));
        assert_eq!(
            instagram_reel_shortcode("https://www.instagram.com/reels/DbHN_ovssLw/"),
            Some("DbHN_ovssLw")
        );
    }

    #[test]
    fn instagram_reel_route_is_not_treated_as_a_profile() {
        assert!(find_download_link("https://www.instagram.com/reel/").is_none());
    }

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

    #[test]
    fn x_video_download_args_concat_playlist_entries() {
        assert_eq!(
            DownloadKind::XVideo.format_args(),
            &[
                "-f",
                "best[ext=mp4][height<=720]/best[height<=720]/mp4",
                "--concat-playlist",
                "always"
            ]
        );
    }

    #[test]
    fn instagram_download_args_request_premerged_mp4_without_warning() {
        assert_eq!(
            DownloadKind::InstagramReel.format_args(),
            &["-f", "b[ext=mp4]"]
        );
    }

    #[test]
    fn x_video_uses_guest_token_instead_of_cookies() {
        assert!(!should_use_cookies(
            "https://x.com/vtchakarova/status/2075991968006439402"
        ));
        assert!(should_use_cookies(
            "https://www.youtube.com/watch?v=Sv5ZZB-M59Q"
        ));
    }

    #[test]
    fn youtube_video_download_args_prefer_1024p_or_best_available() {
        assert_eq!(
            DownloadKind::YouTubeVideo.format_args(),
            &[
                "-f",
                "bestvideo[height<=1024][ext=mp4]+bestaudio[ext=m4a]/bestvideo[height<=1024]+bestaudio/best[height<=1024][ext=mp4]/best[height<=1024]/best",
                "--merge-output-format",
                "mp4"
            ]
        );
    }

    #[test]
    fn youtube_download_args_embed_channel_as_artist_metadata() {
        assert_eq!(
            DownloadKind::YouTubeVideo.metadata_args(),
            &["--embed-metadata", "--parse-metadata", "channel:artist"]
        );
        assert_eq!(
            DownloadKind::YouTubeAudio.metadata_args(),
            &["--embed-metadata", "--parse-metadata", "channel:artist"]
        );
        assert!(DownloadKind::InstagramReel.metadata_args().is_empty());
        assert!(DownloadKind::XVideo.metadata_args().is_empty());
    }

    #[test]
    fn youtube_audio_uses_channel_as_audio_caption() {
        assert_eq!(DownloadKind::YouTubeAudio.caption_field(), Some("channel"));
        assert_eq!(DownloadKind::YouTubeVideo.caption_field(), None);
        assert_eq!(DownloadKind::InstagramReel.caption_field(), None);
    }

    #[test]
    fn youtube_quality_download_args_use_selected_height() {
        assert!(DownloadKind::YouTubeVideo480.format_args()[1].contains("height<=480"));
        assert!(DownloadKind::YouTubeVideo720.format_args()[1].contains("height<=720"));
        assert_eq!(
            DownloadKind::YouTubeVideo1024.format_args(),
            DownloadKind::YouTubeVideo.format_args()
        );
        assert!(DownloadKind::YouTubeVideo1440.format_args()[1].contains("height<=1440"));
        assert!(DownloadKind::YouTubeVideo2160.format_args()[1].contains("height<=2160"));
    }

    #[test]
    fn youtube_watch_link_defaults_to_video_menu() {
        let link = find_download_link("watch https://www.youtube.com/watch?v=Sv5ZZB-M59Q")
            .expect("youtube watch link should be detected");

        assert!(matches!(link.kind, DownloadKind::YouTubeVideo));
        assert!(link.kind.is_youtube());
    }

    #[test]
    fn youtube_live_link_defaults_to_video_menu() {
        let link = find_download_link(
            "stream https://www.youtube.com/live/xskUOJfevBU?is=08Tgh2OuqZm5P4pC",
        )
        .expect("youtube live link should be detected");

        assert!(matches!(link.kind, DownloadKind::YouTubeVideo));
        assert!(link.kind.is_youtube());
        assert_eq!(
            link.url,
            "https://www.youtube.com/live/xskUOJfevBU?is=08Tgh2OuqZm5P4pC"
        );
    }

    #[test]
    fn detects_unsupported_http_links() {
        assert!(contains_http_link(
            "download https://www.tiktok.com/@user/video/123"
        ));
        assert!(contains_http_link("try http://example.com/video"));
        assert!(!contains_http_link("hello, no link here"));
    }

    #[test]
    fn parses_youtube_download_callback_choice() {
        let (kind, id) = parse_youtube_download_callback("yta:abc123").unwrap();
        assert!(matches!(kind, DownloadKind::YouTubeAudio));
        assert_eq!(id, "abc123");

        let (kind, id) = parse_youtube_download_callback("ytv720:abc123").unwrap();
        assert!(matches!(kind, DownloadKind::YouTubeVideo720));
        assert_eq!(id, "abc123");

        let (kind, id) = parse_youtube_download_callback("ytv1440:abc123").unwrap();
        assert!(matches!(kind, DownloadKind::YouTubeVideo1440));
        assert_eq!(id, "abc123");

        let (kind, id) = parse_youtube_download_callback("ytv2160:abc123").unwrap();
        assert!(matches!(kind, DownloadKind::YouTubeVideo2160));
        assert_eq!(id, "abc123");

        let (kind, id) = parse_youtube_download_callback("ytv:abc123").unwrap();
        assert!(matches!(kind, DownloadKind::YouTubeVideo1024));
        assert_eq!(id, "abc123");
        assert!(parse_youtube_download_callback("ig:abc123").is_none());
    }

    #[test]
    fn download_menu_store_expires_abandoned_menus() {
        let now = Instant::now();
        let mut downloads = HashMap::new();
        downloads.insert(
            "expired".into(),
            DownloadMenu {
                url: "https://youtu.be/expired".into(),
                created_at: now - DOWNLOAD_MENU_TTL,
            },
        );

        assert_eq!(take_download_url(&mut downloads, "expired", now), None);
        assert!(downloads.is_empty());
    }

    #[test]
    fn download_menu_store_is_capped() {
        let now = Instant::now();
        let mut downloads = HashMap::new();

        for index in 0..=MAX_DOWNLOAD_MENUS {
            insert_download_menu(
                &mut downloads,
                format!("menu-{index}"),
                format!("https://youtu.be/{index}"),
                now,
            );
        }

        assert_eq!(downloads.len(), MAX_DOWNLOAD_MENUS);
        assert!(downloads.contains_key(&format!("menu-{MAX_DOWNLOAD_MENUS}")));
    }

    #[test]
    fn chunk_count_uses_ceiling_without_extra_exact_multiple() {
        assert_eq!(chunk_count(0, MAX_TG_SIZE), 1);
        assert_eq!(chunk_count(1, MAX_TG_SIZE), 1);
        assert_eq!(chunk_count(MAX_TG_SIZE, MAX_TG_SIZE), 1);
        assert_eq!(chunk_count(MAX_TG_SIZE + 1, MAX_TG_SIZE), 2);
        assert_eq!(chunk_count(MAX_TG_SIZE * 2, MAX_TG_SIZE), 2);
    }

    #[test]
    fn recognizes_request_entity_too_large_for_document_fallback() {
        let too_large = teloxide::RequestError::Api(teloxide::ApiError::RequestEntityTooLarge);
        let other = teloxide::RequestError::Api(teloxide::ApiError::ChatNotFound);

        assert!(is_request_entity_too_large(&too_large));
        assert!(!is_request_entity_too_large(&other));
    }

    #[test]
    fn download_queue_limits_concurrency() {
        let queue = new_download_queue(2);
        let first = Arc::clone(&queue.semaphore).try_acquire_owned().unwrap();
        let second = Arc::clone(&queue.semaphore).try_acquire_owned().unwrap();

        assert!(matches!(
            Arc::clone(&queue.semaphore).try_acquire_owned(),
            Err(TryAcquireError::NoPermits)
        ));

        drop(first);
        assert!(Arc::clone(&queue.semaphore).try_acquire_owned().is_ok());
        drop(second);
    }

    #[test]
    fn parses_positive_max_concurrent_downloads() {
        assert_eq!(parse_max_concurrent_downloads("1"), Ok(1));
        assert_eq!(parse_max_concurrent_downloads("4"), Ok(4));
        assert!(parse_max_concurrent_downloads("0").is_err());
        assert!(parse_max_concurrent_downloads("invalid").is_err());
    }

    #[test]
    fn waiting_position_is_released_when_waiter_is_dropped() {
        let queue = new_download_queue(2);
        let (first_position, first_waiter) = register_waiting_download(&queue);
        let (second_position, _second_waiter) = register_waiting_download(&queue);

        assert_eq!(first_position, 1);
        assert_eq!(second_position, 2);

        drop(first_waiter);
        let (replacement_position, _replacement_waiter) = register_waiting_download(&queue);
        assert_eq!(replacement_position, 2);
    }

    #[tokio::test]
    async fn download_queue_admits_waiters_in_fifo_order() {
        let queue = new_download_queue(1);
        let active = Arc::clone(&queue.semaphore).acquire_owned().await.unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        let first_semaphore = Arc::clone(&queue.semaphore);
        let first_tx = tx.clone();
        let first = tokio::spawn(async move {
            let _permit = first_semaphore.acquire_owned().await.unwrap();
            first_tx.send(1).unwrap();
        });
        tokio::task::yield_now().await;

        let second_semaphore = Arc::clone(&queue.semaphore);
        let second = tokio::spawn(async move {
            let _permit = second_semaphore.acquire_owned().await.unwrap();
            tx.send(2).unwrap();
        });
        tokio::task::yield_now().await;

        drop(active);

        assert_eq!(rx.recv().await, Some(1));
        assert_eq!(rx.recv().await, Some(2));
        first.await.unwrap();
        second.await.unwrap();
    }
}
