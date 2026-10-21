//! Real ffmpeg + isolated HTTP tests. Never contact Telegram.
use super::*;
use std::io::{BufRead, Read, Write};

fn fixture(dir: &Path, video: &str, audio: &str) -> PathBuf {
    let input = dir.join("input.mkv");
    let output = std::process::Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=blue:s=160x120:r=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-t",
            "0.3",
            "-c:v",
            video,
            "-c:a",
            audio,
        ])
        .arg(&input)
        .output()
        .expect("ffmpeg must be installed");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    input
}

// Capture editMessageText and sendVideo requests without real bot credentials.
fn mock_api(
    count: usize,
) -> (
    reqwest::Url,
    std::thread::JoinHandle<Vec<(String, Vec<u8>)>>,
) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let handle = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..count {
            let deadline = Instant::now() + Duration::from_secs(20);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "mock API request timed out");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => panic!("mock API accept: {e}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            reader.read_line(&mut first).unwrap();
            let mut length = 0;
            let mut chunked = false;
            loop {
                let mut line = String::new();
                assert!(
                    reader.read_line(&mut line).unwrap() > 0,
                    "HTTP headers ended early"
                );
                if line == "\r\n" {
                    break;
                }
                let line = line.to_ascii_lowercase();
                if let Some(value) = line.strip_prefix("content-length:") {
                    length = value.trim().parse::<usize>().unwrap();
                }
                chunked |= line.starts_with("transfer-encoding:") && line.contains("chunked");
            }
            let mut body = Vec::new();
            if chunked {
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let size =
                        usize::from_str_radix(line.trim().split(';').next().unwrap(), 16).unwrap();
                    if size == 0 {
                        break;
                    }
                    let offset = body.len();
                    body.resize(offset + size, 0);
                    reader.read_exact(&mut body[offset..]).unwrap();
                    let mut crlf = [0; 2];
                    reader.read_exact(&mut crlf).unwrap();
                }
            } else {
                body.resize(length, 0);
                reader.read_exact(&mut body).unwrap();
            }
            requests.push((first.split_whitespace().nth(1).unwrap().to_string(), body));
            let response = r#"{"ok":true,"result":{"message_id":42,"date":1,"chat":{"id":123,"type":"private"},"text":"ok"}}"#;
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
        }
        requests
    });
    (url, handle)
}

#[tokio::test]
#[ignore = "requires ffmpeg and ffprobe"]
async fn real_video_conversion_and_status_sequence() {
    for (video, audio) in [("libx264", "aac"), ("mpeg4", "libmp3lame")] {
        let dir = std::env::temp_dir().join(format!("fetcher-status-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = fixture(&dir, video, audio);
        let prepared = prepare_video_for_telegram(&input).await.unwrap();
        assert_eq!(
            probe_stream_codec(&prepared, "v:0")
                .await
                .unwrap()
                .as_deref(),
            Some("h264")
        );
        assert_eq!(
            probe_stream_codec(&prepared, "a:0")
                .await
                .unwrap()
                .as_deref(),
            Some("aac")
        );
        std::fs::remove_file(prepared).unwrap();

        let (url, server) = mock_api(3);
        let bot = Bot::new("123:test").set_api_url(url);
        send_video(
            &bot,
            ChatId(123),
            &input,
            "test",
            "https://youtube.com/shorts/test",
            Some(MessageId(42)),
        )
        .await
        .unwrap();
        let requests = server.join().unwrap();
        assert!(
            requests[0].0.ends_with("/EditMessageText"),
            "{}",
            requests[0].0
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&requests[0].1).unwrap()["text"],
            "Converting…"
        );
        assert!(requests[1].0.ends_with("/EditMessageText"));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&requests[1].1).unwrap()["text"],
            "Sending video..."
        );
        assert!(requests[2].0.ends_with("/SendVideo"));
        let files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(
            files,
            vec![input],
            "prepared video and thumbnail must be removed"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[tokio::test]
#[ignore = "requires ffprobe"]
async fn conversion_error_does_not_announce_sending() {
    let (url, server) = mock_api(1);
    let bot = Bot::new("123:test").set_api_url(url);
    let missing = std::env::temp_dir().join(format!("missing-{}.mp4", Uuid::new_v4()));
    let result = send_video(
        &bot,
        ChatId(123),
        &missing,
        "test",
        "https://youtube.com/shorts/test",
        Some(MessageId(42)),
    )
    .await;
    assert!(result.is_err());
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&requests[0].1).unwrap()["text"],
        "Converting…"
    );
}
