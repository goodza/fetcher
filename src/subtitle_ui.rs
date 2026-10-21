//! YouTube subtitle menus; subtitle files and media downloads remain independent.
use super::*;
use youtube_subtitles::SubtitleTrack;

const PAGE_SIZE: usize = 8;

#[derive(Debug, PartialEq)]
pub(super) enum SubtitleChoice<'a> {
    Menu { id: &'a str, page: usize },
    Back(&'a str),
    Download { id: &'a str, index: usize },
}

pub(super) fn parse_callback(data: &str) -> Option<SubtitleChoice<'_>> {
    if data.len() > 64 {
        return None;
    }
    let mut parts = data.split(':');
    let action = parts.next()?;
    let id = parts.next()?;
    Uuid::parse_str(id).ok()?;
    let choice = match action {
        "ytsm" => SubtitleChoice::Menu {
            id,
            page: parts.next()?.parse().ok()?,
        },
        "ytb" => SubtitleChoice::Back(id),
        "yts" => SubtitleChoice::Download {
            id,
            index: parts.next()?.parse().ok()?,
        },
        _ => return None,
    };
    parts.next().is_none().then_some(choice)
}

pub(super) fn menu_text(has_subtitles: bool) -> &'static str {
    if has_subtitles {
        "Choose video, audio or subtitles:"
    } else {
        "Choose video or audio:"
    }
}

pub(super) fn youtube_menu_keyboard(id: &str, has_subtitles: bool) -> InlineKeyboardMarkup {
    let mut rows = vec![vec![
        InlineKeyboardButton::callback("Video", format!("ytv:{id}")),
        InlineKeyboardButton::callback("Audio", format!("yta:{id}")),
    ]];
    if has_subtitles {
        rows.push(vec![InlineKeyboardButton::callback(
            "Subtitles (.srt)",
            format!("ytsm:{id}:0"),
        )]);
    }
    InlineKeyboardMarkup::new(rows)
}

fn subtitle_keyboard(
    id: &str,
    tracks: &[SubtitleTrack],
    page: usize,
) -> Option<InlineKeyboardMarkup> {
    let start = page.checked_mul(PAGE_SIZE)?;
    if start >= tracks.len() {
        return None;
    }
    let mut rows = Vec::new();
    for (offset, track) in tracks.iter().skip(start).take(PAGE_SIZE).enumerate() {
        let name: String = track.name.chars().take(48).collect();
        let label = format!(
            "{} [{}]{}",
            name,
            track.language,
            if track.automatic { " (auto)" } else { "" }
        );
        rows.push(vec![InlineKeyboardButton::callback(
            label,
            format!("yts:{id}:{}", start + offset),
        )]);
    }
    let mut navigation = Vec::new();
    if page > 0 {
        navigation.push(InlineKeyboardButton::callback(
            "← Previous",
            format!("ytsm:{id}:{}", page - 1),
        ));
    }
    if start + PAGE_SIZE < tracks.len() {
        navigation.push(InlineKeyboardButton::callback(
            "Next →",
            format!("ytsm:{id}:{}", page + 1),
        ));
    }
    if !navigation.is_empty() {
        rows.push(navigation);
    }
    rows.push(vec![InlineKeyboardButton::callback(
        "Back to video / audio",
        format!("ytb:{id}"),
    )]);
    Some(InlineKeyboardMarkup::new(rows))
}

fn get_menu(
    downloads: &DownloadStore,
    id: &str,
    chat_id: ChatId,
    now: Instant,
) -> Option<DownloadMenu> {
    let mut menus = downloads.lock().expect("download store lock poisoned");
    prune_download_menus(&mut menus, now);
    menus
        .get(id)
        .filter(|menu| menu.chat_id == Some(chat_id))
        .cloned()
}

async fn expired(bot: &Bot, q: &CallbackQuery) -> ResponseResult<()> {
    bot.answer_callback_query(q.id.clone())
        .text(
            "This subtitle menu expired or the choice is unavailable. Send the YouTube link again.",
        )
        .show_alert(true)
        .await?;
    Ok(())
}

pub(super) async fn handle_callback(
    bot: &Bot,
    q: &CallbackQuery,
    choice: SubtitleChoice<'_>,
    queue: &DownloadQueue,
    downloads: &DownloadStore,
) -> ResponseResult<()> {
    let Some(message) = q.regular_message() else {
        return expired(bot, q).await;
    };
    let chat_id = message.chat.id;
    let id = match choice {
        SubtitleChoice::Menu { id, .. }
        | SubtitleChoice::Back(id)
        | SubtitleChoice::Download { id, .. } => id,
    };
    let Some(menu) = get_menu(downloads, id, chat_id, Instant::now()) else {
        return expired(bot, q).await;
    };
    match choice {
        SubtitleChoice::Menu { page, .. } => {
            let Some(keyboard) = subtitle_keyboard(id, &menu.subtitles, page) else {
                return expired(bot, q).await;
            };
            bot.answer_callback_query(q.id.clone()).await?;
            bot.edit_message_text(
                chat_id,
                message.id,
                "Choose subtitle language (.srt). Auto-generated tracks are marked (auto):",
            )
            .reply_markup(keyboard)
            .await?;
        }
        SubtitleChoice::Back(_) => {
            bot.answer_callback_query(q.id.clone()).await?;
            bot.edit_message_text(chat_id, message.id, menu_text(!menu.subtitles.is_empty()))
                .reply_markup(youtube_menu_keyboard(id, !menu.subtitles.is_empty()))
                .await?;
        }
        SubtitleChoice::Download { index, .. } => {
            let Some(track) = menu.subtitles.get(index) else {
                return expired(bot, q).await;
            };
            bot.answer_callback_query(q.id.clone()).await?;
            // Retain the language menu: the user can also fetch media or another language.
            let status = bot
                .send_message(chat_id, "Downloading subtitles...")
                .await?;
            let result = {
                let _permit = acquire_download_permit(queue, bot, chat_id, status.id).await;
                log_callback_download_link("youtube_subtitles", &menu.url, q).await;
                bot.edit_message_text(chat_id, status.id, "Downloading subtitles...")
                    .await
                    .ok();
                download_and_send_subtitle(bot, chat_id, status.id, &menu.url, track).await
            };
            match result {
                Ok(()) => {
                    bot.delete_message(chat_id, status.id).await.ok();
                }
                Err(error) => {
                    log::warn!("YouTube subtitle download failed: {error}");
                    bot.edit_message_text(
                        chat_id,
                        status.id,
                        format!("Subtitle download failed: {error}"),
                    )
                    .await?;
                }
            }
        }
    }
    Ok(())
}

fn subtitle_filename(track: &SubtitleTrack) -> String {
    let language: String = track
        .language
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(48)
        .collect();
    format!(
        "subtitles.{}.srt",
        if language.is_empty() {
            "und"
        } else {
            &language
        }
    )
}

async fn download_and_send_subtitle(
    bot: &Bot,
    chat_id: ChatId,
    status_msg_id: MessageId,
    url: &str,
    track: &SubtitleTrack,
) -> Result<(), String> {
    let work_dir = std::env::temp_dir().join(format!("fetcher-subs-{}", Uuid::new_v4()));
    tokio::fs::create_dir(&work_dir)
        .await
        .map_err(|_| "Cannot create subtitle working directory".to_string())?;
    let result = async {
        let path = youtube_subtitles::download(url, track, &work_dir).await?;
        let size = tokio::fs::metadata(&path)
            .await
            .map_err(|_| "Cannot read subtitle file".to_string())?
            .len();
        if size > telegram_upload_limit(bot) {
            return Err("Subtitle file exceeds the Telegram upload limit".into());
        }
        bot.edit_message_text(chat_id, status_msg_id, "Sending subtitles...")
            .await
            .ok();
        bot.send_document(
            chat_id,
            InputFile::file(&path).file_name(subtitle_filename(track)),
        )
        .caption(format!(
            "{}{}\n{}",
            track.name,
            if track.automatic {
                " (auto-generated)"
            } else {
                ""
            },
            url
        ))
        .await
        .map_err(|_| "Telegram could not send the subtitle file".to_string())?;
        Ok(())
    }
    .await;
    if let Err(error) = tokio::fs::remove_dir_all(&work_dir).await {
        log::warn!("Cannot clean subtitle working directory: {error}");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    const ID: &str = "aa8f28ab-32d7-407d-ab0e-f8c8e91562be";
    fn track(language: &str, automatic: bool) -> SubtitleTrack {
        SubtitleTrack {
            language: language.into(),
            name: language.into(),
            automatic,
        }
    }

    #[test]
    fn menu_only_offers_subtitles_when_available() {
        for available in [false, true] {
            let value = serde_json::to_value(youtube_menu_keyboard(ID, available)).unwrap();
            let rows = value["inline_keyboard"].as_array().unwrap();
            assert_eq!(rows.len(), if available { 2 } else { 1 });
            assert_eq!(rows[0][0]["text"], "Video");
            assert_eq!(rows[0][1]["text"], "Audio");
            assert_eq!(value.to_string().contains("ytsm:"), available);
        }
    }

    #[test]
    fn parses_subtitle_choices_and_rejects_malformed_callbacks() {
        assert_eq!(
            parse_callback(&format!("ytsm:{ID}:2")),
            Some(SubtitleChoice::Menu { id: ID, page: 2 })
        );
        assert_eq!(
            parse_callback(&format!("yts:{ID}:12")),
            Some(SubtitleChoice::Download { id: ID, index: 12 })
        );
        assert_eq!(
            parse_callback(&format!("ytb:{ID}")),
            Some(SubtitleChoice::Back(ID))
        );
        for data in [
            "yts:bad:0".into(),
            format!("yts:{ID}:-1"),
            format!("ytsm:{ID}:NaN"),
            format!("ytb:{ID}:extra"),
            format!("yts:{ID}:0:extra"),
            format!("yts:{ID}:99999999999999999999999999999999"),
            format!("ytv:{ID}"),
        ] {
            assert!(parse_callback(&data).is_none(), "{data}");
        }
    }

    #[test]
    fn paginates_all_tracks_without_oversized_callback_data() {
        let tracks: Vec<_> = (0..19)
            .map(|n| track(&format!("lang-{n}"), n % 2 == 0))
            .collect();
        let mut indices = Vec::new();
        for page in 0..3 {
            let value =
                serde_json::to_value(subtitle_keyboard(ID, &tracks, page).unwrap()).unwrap();
            for row in value["inline_keyboard"].as_array().unwrap() {
                for button in row.as_array().unwrap() {
                    let data = button["callback_data"].as_str().unwrap();
                    assert!(data.len() <= 64);
                    if let Some(SubtitleChoice::Download { index, .. }) = parse_callback(data) {
                        indices.push(index);
                    }
                }
            }
        }
        assert_eq!(indices, (0..19).collect::<Vec<_>>());
        assert!(subtitle_keyboard(ID, &tracks, 3).is_none());
        assert!(subtitle_keyboard(ID, &tracks, usize::MAX).is_none());
        assert!(subtitle_keyboard(ID, &[], 0).is_none());
        let value = serde_json::to_value(subtitle_keyboard(ID, &tracks, 0).unwrap()).unwrap();
        assert!(value["inline_keyboard"][0][0]["text"]
            .as_str()
            .unwrap()
            .contains("(auto)"));
    }

    #[test]
    fn subtitle_menu_is_chat_bound_expiring_and_non_consuming() {
        let now = Instant::now();
        let downloads: DownloadStore = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut menus = downloads.lock().unwrap();
            insert_download_menu(&mut menus, ID.into(), "https://youtu.be/test".into(), now);
            menus.get_mut(ID).unwrap().chat_id = Some(ChatId(123));
        }
        assert!(get_menu(&downloads, ID, ChatId(456), now).is_none());
        assert!(get_menu(&downloads, ID, ChatId(123), now).is_some());
        assert!(get_menu(&downloads, ID, ChatId(123), now).is_some());
        assert!(get_menu(&downloads, ID, ChatId(123), now + DOWNLOAD_MENU_TTL).is_none());
    }

    fn callback(data: &str) -> CallbackQuery {
        serde_json::from_value(serde_json::json!({
            "id": "test-query", "from": { "id": 123, "is_bot": false, "first_name": "Test" },
            "chat_instance": "test", "data": data,
            "message": { "message_id": 42, "date": 1, "chat": { "id": 123, "type": "private" }, "text": "Menu" }
        })).unwrap()
    }

    #[tokio::test]
    async fn subtitle_navigation_edits_menu_and_retains_media_choices() {
        let (url, server) = crate::media_status_tests::mock_api(4);
        let bot = Bot::new("123:test").set_api_url(url);
        let downloads: DownloadStore = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut menus = downloads.lock().unwrap();
            insert_download_menu(
                &mut menus,
                ID.into(),
                "https://youtu.be/test".into(),
                Instant::now(),
            );
            let menu = menus.get_mut(ID).unwrap();
            menu.chat_id = Some(ChatId(123));
            menu.subtitles = vec![track("ru", true)];
        }
        let queue = new_download_queue(1);
        for data in [format!("ytsm:{ID}:0"), format!("ytb:{ID}")] {
            handle_callback(
                &bot,
                &callback(&data),
                parse_callback(&data).unwrap(),
                &queue,
                &downloads,
            )
            .await
            .unwrap();
        }
        let requests = server.join().unwrap();
        assert!(requests[0].0.ends_with("/AnswerCallbackQuery"));
        let languages: serde_json::Value = serde_json::from_slice(&requests[1].1).unwrap();
        assert_eq!(
            languages["reply_markup"]["inline_keyboard"][0][0]["callback_data"],
            format!("yts:{ID}:0")
        );
        let back: serde_json::Value = serde_json::from_slice(&requests[3].1).unwrap();
        assert_eq!(
            back["reply_markup"]["inline_keyboard"][0][0]["text"],
            "Video"
        );
        assert_eq!(
            back["reply_markup"]["inline_keyboard"][1][0]["text"],
            "Subtitles (.srt)"
        );
        assert!(downloads.lock().unwrap().contains_key(ID));
    }

    #[tokio::test]
    async fn expired_subtitle_callback_is_acknowledged_without_download() {
        let (url, server) = crate::media_status_tests::mock_api(1);
        let bot = Bot::new("123:test").set_api_url(url);
        let downloads: DownloadStore = Arc::new(Mutex::new(HashMap::new()));
        let data = format!("yts:{ID}:0");
        handle_callback(
            &bot,
            &callback(&data),
            parse_callback(&data).unwrap(),
            &new_download_queue(1),
            &downloads,
        )
        .await
        .unwrap();
        let requests = server.join().unwrap();
        let response: serde_json::Value = serde_json::from_slice(&requests[0].1).unwrap();
        assert_eq!(response["show_alert"], true);
        assert!(response["text"].as_str().unwrap().contains("expired"));
    }

    #[tokio::test]
    #[ignore = "live YouTube access, yt-dlp, Node, ffmpeg and cookies required"]
    async fn live_subtitle_document_upload() {
        let url =
            std::env::var("FETCHER_SUBTITLE_TEST_URL").expect("Set FETCHER_SUBTITLE_TEST_URL");
        let tracks = youtube_subtitles::available_tracks(&url).await.unwrap();
        let track = tracks.first().expect("Fixture must have captions");
        let (api_url, server) = crate::media_status_tests::mock_api(2);
        let bot = Bot::new("123:test").set_api_url(api_url);
        download_and_send_subtitle(&bot, ChatId(123), MessageId(42), &url, track)
            .await
            .unwrap();
        let requests = server.join().unwrap();
        assert!(requests[0].0.ends_with("/EditMessageText"));
        let status: serde_json::Value = serde_json::from_slice(&requests[0].1).unwrap();
        assert_eq!(status["text"], "Sending subtitles...");
        assert!(requests[1].0.ends_with("/SendDocument"));
        let multipart = String::from_utf8_lossy(&requests[1].1);
        assert!(multipart.contains(&subtitle_filename(track)));
        assert!(
            multipart.contains("-->"),
            "Uploaded document must contain real SRT timestamps"
        );
        println!(
            "Real subtitle file uploaded to isolated mock API: {} bytes",
            requests[1].1.len()
        );
    }

    #[test]
    fn subtitle_filename_cannot_include_paths_or_control_characters() {
        assert_eq!(
            subtitle_filename(&track("en-US", false)),
            "subtitles.en-US.srt"
        );
        assert_eq!(
            subtitle_filename(&track("../../ru\n", false)),
            "subtitles.ru.srt"
        );
        assert_eq!(subtitle_filename(&track("/", false)), "subtitles.und.srt");
    }
}
