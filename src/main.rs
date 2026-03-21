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
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "info");
    }
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
            "--cookies-from-browser", cookie_browser(),
            "-o", output.to_str().unwrap(),
            url,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("Failed to run yt-dlp: {e}"))?;

    let stderr = child.stderr.take().unwrap();
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            log::warn!("[yt-dlp stderr] {}", line);
        }
    });

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout).lines();

    let progress_re = Regex::new(r"\[download\]\s+(\d+\.?\d*%\s+.*)").unwrap();
    let mut last_update = Instant::now();
    let mut last_text = String::new();
    let update_interval = Duration::from_secs(3);

    while let Ok(Some(line)) = reader.next_line().await {
        log::info!("[yt-dlp] {}", line);
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

const MAX_TG_SIZE: u64 = 49 * 1024 * 1024;

async fn send_audio(bot: &Bot, chat_id: ChatId, path: &PathBuf) -> Result<(), String> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("Cannot read downloaded file: {e}"))?;

    if metadata.len() <= MAX_TG_SIZE {
        bot.send_audio(chat_id, InputFile::file(path))
            .await
            .map_err(|e| format!("Telegram API error: {e}"))?;
        return Ok(());
    }

    let chunks = split_media(path, "mp3").await?;
    for (i, chunk) in chunks.iter().enumerate() {
        log::info!("Sending audio chunk {}/{}", i + 1, chunks.len());
        bot.send_audio(chat_id, InputFile::file(chunk))
            .await
            .map_err(|e| format!("Telegram API error on chunk {}: {e}", i + 1))?;
    }
    for chunk in &chunks {
        let _ = tokio::fs::remove_file(chunk).await;
    }

    Ok(())
}

async fn send_video(bot: &Bot, chat_id: ChatId, path: &PathBuf) -> Result<(), String> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("Cannot read downloaded file: {e}"))?;

    if metadata.len() <= MAX_TG_SIZE {
        bot.send_video(chat_id, InputFile::file(path))
            .await
            .map_err(|e| format!("Telegram API error: {e}"))?;
        return Ok(());
    }

    let chunks = split_media(path, "mp4").await?;
    for (i, chunk) in chunks.iter().enumerate() {
        log::info!("Sending video chunk {}/{}", i + 1, chunks.len());
        bot.send_video(chat_id, InputFile::file(chunk))
            .await
            .map_err(|e| format!("Telegram API error on chunk {}: {e}", i + 1))?;
    }
    for chunk in &chunks {
        let _ = tokio::fs::remove_file(chunk).await;
    }

    Ok(())
}

async fn split_media(path: &PathBuf, ext: &str) -> Result<Vec<PathBuf>, String> {
    let file_size = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("Cannot read file: {e}"))?
        .len();

    let num_chunks = (file_size / MAX_TG_SIZE) + 1;

    // Get total duration via ffprobe
    let probe = tokio::process::Command::new("ffprobe")
        .args([
            "-v", "error",
            "-show_entries", "format=duration",
            "-of", "default=noprint_wrappers=1:nokey=1",
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
            "-i", path.to_str().unwrap(),
            "-f", "segment",
            "-segment_time", &format!("{chunk_duration:.2}"),
            "-c", "copy",
            "-reset_timestamps", "1",
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
