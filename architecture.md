---
id: fetcher-architecture
type: architecture-design
status: draft
title: Fetcher architecture and operating boundaries
parent: fetcher-goals
---

# Fetcher architecture

## Topology and boundaries

Fetcher has two runtime boundaries: `fetcher-bot`, a Rust/Tokio executable using teloxide's dispatcher, and `fetcher-cookie-bridge`, a dependency-free Python service on the operator's Firefox PC plus a one-shot refresh client on the bot host. There is no application database or durable job queue.

The bot receives Telegram messages and callbacks, runs yt-dlp subprocesses, prepares media with FFmpeg/ffprobe, and uploads through the Telegram Bot API. Instagram profile enumeration additionally starts a loopback ChromeDriver/headless Chrome session; post photos use yt-dlp metadata with HTTP/page-payload fallbacks. Node and yt-dlp's remote EJS component support YouTube extraction.

The deployment described in README runs the bot on gsweb under PM2. Cookie requests traverse a loopback FRPC STCP visitor, an existing FRPS relay, and the PC FRPC proxy to the loopback Python service. FRP and PM2 configuration is private runtime state, not generated or checked in by this repository.

## Dependency edges

- The bot optionally calls the cookie bridge over authenticated HTTP; the bridge reads Firefox SQLite state and has no dependency on Telegram or the Rust executable. The one-shot Python client uses the same endpoint independently of the bot.
- Inside the bot, the root orchestration owns menus, the shared job semaphore, Telegram delivery, and media subprocesses. Subtitle UI uses that shared orchestration and the subtitle backend; the backend uses cookie policy/refresh helpers, not Telegram delivery.
- The refresh client uses the root cookie-path lookup and HTTP/filesystem operations, not Telegram. Retry wrappers are shared by buffered yt-dlp commands and progress-streamed downloads.

These are observed call boundaries, not independently packaged libraries or compiler-enforced architectural layers. The Rust components stay in one `fetcher-bot` spec because they share root-owned state and helpers; directory/file count does not define extra spec nodes.

## Decisions and invariants

External tools own platform extraction and media transforms; the bot owns coordination and user-visible outcomes. **Unconfirmed rationale:** keeping one executable avoids a separate worker service for the trusted-use workload; neither source nor docs establishes why that deployment shape was originally chosen.

Use a shared concurrency permit for subtitle discovery and for download-through-upload work, rather than launching unlimited active external-tool jobs. Use UUID-based working paths to separate concurrent operations. Detailed admission, retry, and cleanup limits belong to `fetcher-bot`.

Prefer actual H.264/AAC streams and normalize video for Telegram playback: MP4 alone does not guarantee compatible codecs. X deliberately bypasses browser cookies because stale authentication can break otherwise working guest extraction. Profile scraping collects links before each scroll because Instagram virtualizes long grids. These rationales are stated in source comments, not inferred new requirements.

Credential export is opt-in, private, bounded, and fail-open for downloading. The endpoint contract belongs to `fetcher-cookie-bridge`; the automatic refresh/cache and installation policy belongs to `fetcher-bot`. A successful export does not prove a platform will accept the session.

## Configuration and delivery

The executable loads `.env` and requires `TG_TOKEN` and a positive integer `MAX_CONCURRENT_DOWNLOADS`. `TELOXIDE_API_URL` defaults to Telegram's cloud API. `COOKIE_REFRESH_URL` enables automatic refresh; `COOKIE_REFRESH_TOKEN_FILE` supplies its bearer token. `ALERT_URL` enables optional error alerts. Browser overrides are `CHROMEDRIVER`, `CHROME_BINARY`, and whitespace-split `INSTAGRAM_CHROME_ARGS`.

The local Bot API compose service binds host port 8081 only on loopback, uses `API_ID`/`API_HASH`, and keeps its own Docker volume. Selecting that API for the bot still requires setting `TELOXIDE_API_URL`; running Compose alone does not change the bot's default.

GitHub pushes to `main` and manual workflow dispatch run Python tests, Rust tests, a release build, checksum-verified upload, and PM2 restart on gsweb. Deployment starts the local Bot API, performs a one-time cloud logout guarded by a marker, and restores the previous binary if restart fails or the process immediately exits. This is a process-liveness check, not a live download acceptance test.

## Evidence and documentation drift

The implementation, tests, Cargo manifest, Compose file, workflow, and README inform these draft specs. No existing durable design document was adopted. README remains operational input; the local Android implementation plan is not a spec.

The ignored local `CLAUDE.md` describes an Instagram-Reels-only `teloxide::repl` bot using `TELOXIDE_TOKEN` and says all logic is in `main.rs`. Current code instead uses a dispatcher, `TG_TOKEN`, additional modules, and the capabilities in `fetcher-goals`. Those stale statements are corrected in this graph; the ignored agent file itself is unchanged.
