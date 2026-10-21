//! Optional YouTube caption discovery and subtitle-only downloads.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;
use tokio::process::Command;

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(90);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Clone, Debug)]
pub(super) struct SubtitleTrack {
    // Keep yt-dlp's actual key (including -orig) for exact selection at download time.
    pub(super) language: String,
    pub(super) name: String,
    pub(super) automatic: bool,
}

pub(super) async fn available_tracks(url: &str) -> Result<Vec<SubtitleTrack>, String> {
    let output = run_command(
        discovery_command(url),
        DISCOVERY_TIMEOUT,
        "Subtitle discovery",
    )
    .await?;
    let info: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| "Subtitle discovery returned invalid metadata.".to_string())?;
    // A captionless video is a normal availability result, not a probe failure.
    Ok(parse_tracks(&info))
}

pub(super) async fn download(
    url: &str,
    track: &SubtitleTrack,
    work_dir: &Path,
) -> Result<PathBuf, String> {
    if !valid_language(&track.language) {
        return Err("Invalid subtitle language selection.".into());
    }
    std::fs::create_dir_all(work_dir)
        .map_err(|_| "Could not create the subtitle working directory.".to_string())?;
    let root = std::fs::canonicalize(work_dir)
        .map_err(|_| "Could not access the subtitle working directory.".to_string())?;
    // Isolate each attempt: never return a stale SRT left by an earlier request.
    // Metadata never participates in the output basename or directory name.
    let directory = root.join(format!("subtitles-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory)
        .map_err(|_| "Could not create the subtitle download directory.".to_string())?;
    let result = async {
        run_command(
            download_command(url, track, &directory),
            DOWNLOAD_TIMEOUT,
            "Subtitle download",
        )
        .await?;
        find_srt(&directory)
    }
    .await;
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&directory);
    }
    result
}

async fn run_command(
    mut command: Command,
    deadline: Duration,
    operation: &str,
) -> Result<Output, String> {
    // Dropping output() on a timeout/cancel also drops and kills the owned child.
    command.kill_on_drop(true);
    let output = tokio::time::timeout(deadline, command.output())
        .await
        .map_err(|_| format!("{operation} timed out. Please try again later."))?
        .map_err(|_| format!("{operation} could not start yt-dlp or read its output."))?;
    if !output.status.success() {
        // yt-dlp stderr/JSON can contain signed URLs and cookie paths. Never expose them.
        return Err(format!(
            "{operation} failed. The captions may be unavailable; please try again later."
        ));
    }
    Ok(output)
}

fn parse_tracks(info: &serde_json::Value) -> Vec<SubtitleTrack> {
    let mut tracks: BTreeMap<String, SubtitleTrack> = BTreeMap::new();
    // Manual first: an automatic alias must never replace a manual track.
    for (field, automatic) in [("subtitles", false), ("automatic_captions", true)] {
        let Some(languages) = info.get(field).and_then(serde_json::Value::as_object) else {
            continue;
        };
        for (language, formats) in languages {
            if !valid_language(language) {
                continue;
            }
            let Some(format) = formats
                .as_array()
                .and_then(|formats| formats.iter().find(|format| is_native_format(format)))
            else {
                continue;
            };
            // yt-dlp emits both en and en-orig for the same native auto captions.
            // Deduplicate by real language, preferring the explicit original alias.
            let key = if automatic {
                language.strip_suffix("-orig").unwrap_or(language)
            } else {
                language.as_str()
            };
            if key.is_empty() {
                continue;
            }
            let replace = match tracks.get(key) {
                None => true,
                Some(existing) => existing.automatic && language.ends_with("-orig"),
            };
            if !replace {
                continue;
            }
            let name = format
                .get("name")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or(key);
            tracks.insert(
                key.to_string(),
                SubtitleTrack {
                    language: language.clone(),
                    name: name.to_string(),
                    automatic,
                },
            );
        }
    }
    tracks.into_values().collect()
}

fn is_native_format(format: &serde_json::Value) -> bool {
    let Some(extension) = format.get("ext").and_then(serde_json::Value::as_str) else {
        return false;
    };
    if extension.trim().is_empty() {
        return false;
    }
    let Some(url) = format
        .get("url")
        .and_then(serde_json::Value::as_str)
        .and_then(|url| reqwest::Url::parse(url).ok())
    else {
        return false;
    };
    matches!(url.scheme(), "http" | "https") && url.host_str().is_some()
        // query_pairs decodes encoded keys as well; even an empty tlang is translated.
        && !url.query_pairs().any(|(key, _)| key == "tlang")
}

fn common_command() -> Command {
    let mut command = Command::new("yt-dlp");
    command.args([
        "--ignore-config",
        "--skip-download",
        "--no-playlist",
        "--no-progress",
        "--js-runtimes",
        "node",
        "--remote-components",
        "ejs:github",
        "--socket-timeout",
        "20",
        "--retries",
        "1",
        "--fragment-retries",
        "1",
    ]);
    super::add_cookie_args(&mut command);
    command.kill_on_drop(true).stderr(Stdio::null());
    command
}

fn discovery_command(url: &str) -> Command {
    let mut command = common_command();
    command
        .args(["--dump-single-json", "--", url])
        .stdout(Stdio::piped());
    command
}

fn download_command(url: &str, track: &SubtitleTrack, directory: &Path) -> Command {
    let mut command = common_command();
    if track.automatic {
        command.args(["--write-auto-subs", "--no-write-subs"]);
    } else {
        command.args(["--write-subs", "--no-write-auto-subs"]);
    }
    command
        .args([
            "--sub-langs",
            &format!("^{}$", regex::escape(&track.language)),
            "--sub-format",
            "srt/vtt/best",
            "--convert-subs",
            "srt",
            "--output",
        ])
        .arg(directory.join("subtitle.%(ext)s"))
        .args(["--", url])
        .stdout(Stdio::null());
    command
}

fn valid_language(language: &str) -> bool {
    !language.is_empty() && language != "live_chat"
        // yt-dlp splits --sub-langs on commas before compiling regexes.
        && !language.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, ',' | '/' | '\\'))
}

fn find_srt(directory: &Path) -> Result<PathBuf, String> {
    let missing = || "Subtitle download produced no nonempty SRT file.".to_string();
    let root = std::fs::canonicalize(directory).map_err(|_| missing())?;
    let entries = std::fs::read_dir(&root).map_err(|_| missing())?;
    let mut found = None;
    for entry in entries {
        let entry = entry.map_err(|_| missing())?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("srt") {
            continue;
        }
        let metadata = std::fs::symlink_metadata(&path).map_err(|_| missing())?;
        if !metadata.is_file() || metadata.len() == 0 {
            continue;
        }
        let path = std::fs::canonicalize(path).map_err(|_| missing())?;
        if path.parent() != Some(root.as_path()) {
            continue;
        }
        if found.replace(path).is_some() {
            return Err("Subtitle download unexpectedly produced multiple SRT files.".into());
        }
    }
    found.ok_or_else(missing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn caption(name: &str, url: &str) -> serde_json::Value {
        json!([{"name": name, "url": url, "ext": "vtt"}])
    }

    fn args(command: &Command) -> Vec<String> {
        command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn assert_pair(args: &[String], key: &str, value: &str) {
        assert!(
            args.windows(2).any(|pair| pair == [key, value]),
            "missing {key} {value}: {args:?}"
        );
    }

    #[test]
    fn manual_native_automatic_and_manual_preference() {
        let info = json!({
            "subtitles": {
                "en": caption("English", "https://www.youtube.com/api/timedtext?lang=en"),
                "fr": caption("French", "https://www.youtube.com/api/timedtext?lang=fr")
            },
            "automatic_captions": {
                "en-orig": caption("English (Original)", "https://www.youtube.com/api/timedtext?lang=en&kind=asr"),
                "de": caption("German", "https://www.youtube.com/api/timedtext?lang=de&kind=asr"),
                "de-orig": caption("German (Original)", "https://www.youtube.com/api/timedtext?lang=de&kind=asr")
            }
        });
        let tracks = parse_tracks(&info);
        assert_eq!(
            tracks
                .iter()
                .map(|t| (t.language.as_str(), t.automatic))
                .collect::<Vec<_>>(),
            [("de-orig", true), ("en", false), ("fr", false)]
        );
        assert_eq!(tracks[1].name, "English");
    }

    #[test]
    fn excludes_translations_and_live_chat_in_both_maps() {
        let info = json!({
            "subtitles": {
                "live_chat": caption("Chat", "https://www.youtube.com/api/timedtext?lang=en"),
                "es": caption("Spanish", "https://www.youtube.com/api/timedtext?lang=en&tlang=es")
            },
            "automatic_captions": {
                "live_chat": caption("Chat", "https://www.youtube.com/api/timedtext?lang=en"),
                "es": caption("Spanish", "https://www.youtube.com/api/timedtext?lang=en&tlang=es"),
                "fr-orig": caption("Mislabelled", "https://www.youtube.com/api/timedtext?lang=en&%74lang=fr"),
                "it": caption("Empty translation", "https://www.youtube.com/api/timedtext?lang=en&tlang="),
                "en-orig": caption("English", "https://www.youtube.com/api/timedtext?lang=en&kind=asr")
            }
        });
        let tracks = parse_tracks(&info);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].language, "en-orig");
    }

    #[test]
    fn invalid_and_empty_entries_are_ignored() {
        let info = json!({"subtitles": {
            "": caption("Empty language", "https://example.test/sub"),
            "en": [], "fr": null, "de": {}, "nl": [null, 5, {}],
            "es": [{"url": "", "ext": "vtt"}],
            "ru": [{"url": "not a URL", "ext": "vtt"}],
            "ja": [{"url": "file:///etc/passwd", "ext": "vtt"}],
            "pt": [{"url": "https://example.test/sub", "ext": ""}],
            "../en": caption("Path", "https://example.test/sub"),
            "en,all": caption("List", "https://example.test/sub")
        }, "automatic_captions": false});
        assert!(parse_tracks(&info).is_empty());
        for info in [json!(null), json!({}), json!({"subtitles": []})] {
            assert!(parse_tracks(&info).is_empty());
        }
    }

    #[test]
    fn native_format_after_a_translated_format_is_still_usable() {
        let info = json!({"automatic_captions": {"en": [
            {"ext": "vtt", "url": "https://example.test/sub?lang=fr&tlang=en"},
            {"ext": "vtt", "url": "https://example.test/sub?lang=en", "name": "English"}
        ]}});
        let tracks = parse_tracks(&info);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].name, "English");
        assert!(tracks[0].automatic);
    }

    #[test]
    fn missing_or_blank_name_falls_back_to_language_and_sort_is_stable() {
        let info = json!({"subtitles": {
            "zh-Hans": [{"ext": "vtt", "url": "https://example.test/sub", "name": " "}],
            "de": [{"ext": "vtt", "url": "https://example.test/sub"}],
            "ar": caption("Arabic", "https://example.test/sub")
        }});
        let tracks = parse_tracks(&info);
        assert_eq!(
            tracks
                .iter()
                .map(|t| t.language.as_str())
                .collect::<Vec<_>>(),
            ["ar", "de", "zh-Hans"]
        );
        assert_eq!(tracks[1].name, "de");
        assert_eq!(tracks[2].name, "zh-Hans");
    }

    #[test]
    fn discovery_is_json_and_never_downloads_media() {
        let args = args(&discovery_command("https://www.youtube.com/shorts/example"));
        for flag in [
            "--dump-single-json",
            "--skip-download",
            "--no-playlist",
            "--ignore-config",
        ] {
            assert!(args.iter().any(|arg| arg == flag), "missing {flag}");
        }
        assert_pair(&args, "--js-runtimes", "node");
        assert_pair(&args, "--remote-components", "ejs:github");
        assert_eq!(
            &args[args.len() - 2..],
            ["--", "https://www.youtube.com/shorts/example"]
        );
    }

    #[test]
    fn manual_and_automatic_commands_are_exclusive_and_subtitle_only() {
        for automatic in [false, true] {
            let track = SubtitleTrack {
                language: "en-orig".into(),
                name: "../../unsafe title".into(),
                automatic,
            };
            let args = args(&download_command(
                "https://www.youtube.com/watch?v=example",
                &track,
                Path::new("/safe/work"),
            ));
            for flag in ["--skip-download", "--no-playlist", "--ignore-config"] {
                assert!(args.iter().any(|arg| arg == flag));
            }
            let (yes, no, opposite) = if automatic {
                ("--write-auto-subs", "--no-write-subs", "--write-subs")
            } else {
                ("--write-subs", "--no-write-auto-subs", "--write-auto-subs")
            };
            assert!(args.iter().any(|arg| arg == yes));
            assert!(args.iter().any(|arg| arg == no));
            assert!(!args.iter().any(|arg| arg == opposite));
            assert_pair(&args, "--sub-format", "srt/vtt/best");
            assert_pair(&args, "--convert-subs", "srt");
            assert_pair(&args, "--sub-langs", "^en\\-orig$");
            assert_pair(&args, "--js-runtimes", "node");
            assert_pair(&args, "--remote-components", "ejs:github");
            assert_pair(&args, "--output", "/safe/work/subtitle.%(ext)s");
            assert!(!args.iter().any(|arg| arg.contains("unsafe title")));
        }
    }

    #[test]
    fn exact_language_expression_cannot_select_other_tracks() {
        let language = "en.*(US)+[x]$";
        let track = SubtitleTrack {
            language: language.into(),
            name: "ignored".into(),
            automatic: false,
        };
        let args = args(&download_command("url", &track, Path::new("/safe")));
        let index = args.iter().position(|arg| arg == "--sub-langs").unwrap();
        let expression = &args[index + 1];
        assert_eq!(expression, &format!("^{}$", regex::escape(language)));
        let regex = regex::Regex::new(expression).unwrap();
        assert!(regex.is_match(language));
        for other in ["en", "en-US", "en-orig", "fr", "en123USx"] {
            assert!(!regex.is_match(other));
        }
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let directory = std::env::temp_dir()
                .join(format!("fetcher-subtitle-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&directory).unwrap();
            Self(directory)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn srt_result_must_be_nonempty_regular_unique_and_inside_directory() {
        let directory = TestDirectory::new();
        assert!(find_srt(&directory.0).is_err());
        std::fs::write(directory.0.join("empty.srt"), "").unwrap();
        std::fs::write(directory.0.join("subtitle.en.vtt"), "WEBVTT").unwrap();
        std::fs::create_dir(directory.0.join("directory.srt")).unwrap();
        assert!(find_srt(&directory.0).is_err());
        let expected = directory.0.join("subtitle.en.srt");
        std::fs::write(&expected, "1\n00:00:00,000 --> 00:00:01,000\nHello\n").unwrap();
        assert_eq!(
            find_srt(&directory.0).unwrap(),
            std::fs::canonicalize(expected).unwrap()
        );
        std::fs::write(directory.0.join("second.srt"), "not empty").unwrap();
        assert!(find_srt(&directory.0).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn srt_result_rejects_symlinks() {
        let directory = TestDirectory::new();
        let outside = TestDirectory::new();
        let target = outside.0.join("outside.srt");
        std::fs::write(&target, "not empty").unwrap();
        std::os::unix::fs::symlink(&target, directory.0.join("subtitle.en.srt")).unwrap();
        assert!(find_srt(&directory.0).is_err());
    }

    /// Requires yt-dlp, Node, ffmpeg and the parent's normal cookies setup.
    /// FETCHER_SUBTITLE_TEST_URL=https://www.youtube.com/watch?v=... \
    ///   cargo test youtube_subtitles::tests::live_subtitle_download -- --ignored --nocapture
    /// Optionally also set FETCHER_SUBTITLE_TEST_NO_TRACKS_URL to a known captionless video.
    #[tokio::test]
    #[ignore = "live YouTube access, yt-dlp, Node, ffmpeg and cookies required"]
    async fn live_subtitle_download() {
        let url = std::env::var("FETCHER_SUBTITLE_TEST_URL")
            .expect("Set FETCHER_SUBTITLE_TEST_URL to a YouTube video or Short with captions");
        let tracks = available_tracks(&url)
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        let track = tracks
            .first()
            .expect("Fixture must have manual or native automatic captions");
        let directory = TestDirectory::new();
        let path = download(&url, track, &directory.0)
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        assert!(path.starts_with(std::fs::canonicalize(&directory.0).unwrap()));
        assert_eq!(path.extension().and_then(|ext| ext.to_str()), Some("srt"));
        let contents = std::fs::read_to_string(&path).expect("SRT must be readable text");
        assert!(!contents.is_empty());
        assert!(
            contents.contains("-->"),
            "SRT must contain timestamp arrows"
        );

        fn assert_caption_files_only(directory: &Path) {
            for entry in std::fs::read_dir(directory).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    assert_caption_files_only(&entry.path());
                } else {
                    let path = entry.path();
                    assert!(
                        matches!(
                            path.extension().and_then(|ext| ext.to_str()),
                            Some("srt" | "vtt" | "ttml" | "srv1" | "srv2" | "srv3" | "json3")
                        ),
                        "Subtitle-only download must not leave video, audio or other files"
                    );
                }
            }
        }
        assert_caption_files_only(&directory.0);
        // Print only non-sensitive verification facts, never URLs/track JSON/stderr.
        println!(
            "Live subtitle download: {} tracks, nonempty SRT ({} bytes), no media files",
            tracks.len(),
            contents.len()
        );

        if let Ok(url) = std::env::var("FETCHER_SUBTITLE_TEST_NO_TRACKS_URL") {
            assert!(
                available_tracks(&url)
                    .await
                    .expect("Captionless fixture must still extract successfully")
                    .is_empty(),
                "Captionless fixture unexpectedly has captions"
            );
            println!("Captionless fixture: empty availability result");
        }
    }

    #[test]
    fn language_validation_rejects_paths_lists_and_control_characters() {
        for language in ["", " ", "en,fr", "../en", "en\\fr", "en\n", "live_chat"] {
            assert!(!valid_language(language), "accepted {language:?}");
        }
        for language in ["en", "en-orig", "zh-Hans", "en.*(US)"] {
            assert!(valid_language(language));
        }
    }
}
