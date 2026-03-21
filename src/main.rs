use std::path::PathBuf;
use std::time::Duration;

use regex::Regex;
use teloxide::prelude::*;
use teloxide::types::{InputFile, MessageId};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::time::Instant;
use uuid::Uuid;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    pretty_env_logger::init();
    log::info!("Starting fetcher bot...");

    let token = std::env::var("TG_TOKEN").expect("TG_TOKEN must be set");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("Failed to build HTTP client");
    let bot = Bot::with_client(token, client);
    teloxide::repl(bot, handle_message).await;
}

async fn handle_message(bot: Bot, msg: Message) -> ResponseResult<()> {
    let text = match msg.text() {
        Some(t) => t,
        None => return Ok(()),
    };

    if let Some(user) = msg.from() {
        log::info!("Message from user id: {}", user.id);
    }

    let ig_re = Regex::new(r"https?://(?:www\.)?instagram\.com/(?:reel|reels)/[A-Za-z0-9_-]+")
        .unwrap();
    let yt_re = Regex::new(r"https?://(?:(?:www\.|m\.)?youtube\.com/watch\?[^\s]*v=[A-Za-z0-9_-]+|youtu\.be/[A-Za-z0-9_-]+)")
        .unwrap();

    if let Some(m) = ig_re.find(text) {
        let url = m.as_str();
        let tmp_path = std::env::temp_dir().join(format!("{}.mp4", Uuid::new_v4()));

        let status_msg = bot.send_message(msg.chat.id, "Downloading reel...").await?;

        match download_with_progress(url, &tmp_path, &["-f", "mp4"], &bot, msg.chat.id, status_msg.id).await {
            Ok(()) => {
                bot.edit_message_text(msg.chat.id, status_msg.id, "Sending video...")
                    .await
                    .ok();
                if let Err(e) = send_video(&bot, msg.chat.id, &tmp_path).await {
                    bot.edit_message_text(msg.chat.id, status_msg.id, format!("Failed to send video: {e}"))
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
    } else if let Some(m) = yt_re.find(text) {
        let url = m.as_str();
        let tmp_path = std::env::temp_dir().join(format!("{}.mp3", Uuid::new_v4()));

        let status_msg = bot.send_message(msg.chat.id, "Downloading audio...").await?;

        match download_with_progress(url, &tmp_path, &["-x", "--audio-format", "mp3"], &bot, msg.chat.id, status_msg.id).await {
            Ok(()) => {
                bot.edit_message_text(msg.chat.id, status_msg.id, "Sending audio...")
                    .await
                    .ok();
                if let Err(e) = send_audio(&bot, msg.chat.id, &tmp_path).await {
                    bot.edit_message_text(msg.chat.id, status_msg.id, format!("Failed to send audio: {e}"))
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
    }

    Ok(())
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

async fn download_with_progress(
    url: &str,
    output: &PathBuf,
    format_args: &[&str],
    bot: &Bot,
    chat_id: ChatId,
    msg_id: MessageId,
) -> Result<(), String> {
    let mut cmd = tokio::process::Command::new("yt-dlp");
    cmd.args(format_args)
        .args([
            "--newline",
            "--max-filesize", "50m",
            "--cookies-from-browser", cookie_browser(),
            "-o", output.to_str().unwrap(),
            url,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("Failed to run yt-dlp: {e}"))?;

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout).lines();

    let progress_re = Regex::new(r"\[download\]\s+(\d+\.?\d*%\s+.*)").unwrap();
    let mut last_update = Instant::now();
    let mut last_text = String::new();
    let update_interval = Duration::from_secs(3);

    while let Ok(Some(line)) = reader.next_line().await {
        if let Some(caps) = progress_re.captures(&line) {
            let progress = caps.get(1).unwrap().as_str().to_string();
            // Throttle edits to avoid Telegram rate limits
            if last_update.elapsed() >= update_interval && progress != last_text {
                let display = format!("Downloading...\n{progress}");
                bot.edit_message_text(chat_id, msg_id, &display).await.ok();
                last_text = progress;
                last_update = Instant::now();
            }
        }
    }

    let status = child.wait().await.map_err(|e| format!("yt-dlp error: {e}"))?;

    if !status.success() {
        return Err("yt-dlp exited with an error".into());
    }

    Ok(())
}

async fn send_audio(bot: &Bot, chat_id: ChatId, path: &PathBuf) -> Result<(), String> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("Cannot read downloaded file: {e}"))?;

    if metadata.len() > 50 * 1024 * 1024 {
        return Err("Audio exceeds Telegram's 50 MB limit".into());
    }

    bot.send_audio(chat_id, InputFile::file(path))
        .await
        .map_err(|e| format!("Telegram API error: {e}"))?;

    Ok(())
}

async fn send_video(bot: &Bot, chat_id: ChatId, path: &PathBuf) -> Result<(), String> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("Cannot read downloaded file: {e}"))?;

    if metadata.len() > 50 * 1024 * 1024 {
        return Err("Video exceeds Telegram's 50 MB limit".into());
    }

    bot.send_video(chat_id, InputFile::file(path))
        .await
        .map_err(|e| format!("Telegram API error: {e}"))?;

    Ok(())
}
