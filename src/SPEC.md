---
id: fetcher-bot
type: module-design
status: active
title: Fetcher bot runtime and media contracts
parent: fetcher-architecture
depends-on: [fetcher-cookie-bridge]
---

# Bot runtime

## Responsibility and boundary

Own Telegram interaction, job admission, temporary media, extraction, conversion, and delivery. The private Rust helper modules are part of this executable, not a public library. Runtime dependency edges and configuration live in `fetcher-architecture`; the optional external export protocol lives in `fetcher-cookie-bridge`.

Do not equate the trusted-use audience in `fetcher-goals` with authorization: the bot has no user allowlist, per-user quota, or tenant isolation. It handles message text, not arbitrary attachment captions or a general-purpose URL download API.

## Requests and menu state

Link recognition selects one supported link by route priority, not every URL in a message: Instagram Reel, post, profile, X status, YouTube Shorts, then other supported YouTube forms. Unrecognized HTTP links receive an unsupported-link reply; non-link text and non-text messages are ignored. Inline queries return personal, uncached guidance without downloading.

All YouTube kinds offer video/audio after subtitle discovery; discovery failure leaves media choices available. Instagram profiles offer Reels/photos/both. Menus expire after 30 minutes and the shared store holds at most 1,024, evicting the oldest on insertion. YouTube media/subtitle callbacks are chat-bound and reusable until expiry. Instagram profile callbacks consume their menu and are not chat-bound in the store; when a photos-only choice finds no photos, a replacement profile menu is offered.

The positive configured semaphore limit caps active work. A contended request gets a queue-position snapshot, and semaphore acquisition is registered before awaiting that status edit to preserve FIFO admission. Discovery takes a permit; a download retains its permit through conversion and Telegram upload. Waiting-count guards release their count when dropped. **Confirmed limitation:** waiting requests have no explicit cap; menus and pending jobs do not survive restart.

## Media extraction and delivery

For Instagram single photos, accept yt-dlp's best thumbnail only for a matching, format-less single post, never a video/playlist cover. Fall back to embed and canonical-page JSON payloads for photos/carousels, preserve order, deduplicate URLs, select the largest available image, and skip video children. Send photos individually or in groups of at most ten; Telegram RetryAfter responses allow up to four send attempts with the requested delay plus one second.

For profiles, collect canonical links throughout browser scrolling, deduplicated by shortcode. Require six unchanged height/link rounds to consider the bottom stable; stop with an incomplete-list error if 600 rounds do not stabilize. A login redirect is an error. Download discovered Reels sequentially, combining multiple Reels with circle-open transitions and matching audio fades (at most 0.5 seconds, bounded by half the shortest clip); supply silence for missing audio. In a combined request, an absent media category is not by itself a failure. Photo posts that cannot be processed may be skipped, but reported failures after partial delivery produce an incomplete-result error rather than silent success.

Video delivery inspects codecs and prepares fast-start MP4 with H.264 video and AAC audio when present, copying compatible streams. Announce conversion before preparation and sending only after preparation succeeds. Preview generation is best-effort. A Telegram RequestEntityTooLarge response to video delivery retries that file as a document; other Telegram errors are not covered by that fallback.

Upload policy uses 49 MiB for `api.telegram.org` and 2,000,000,000 bytes for every other configured API host. The latter is an endpoint classification heuristic, not a loopback/security check. Bot HTTP request timeouts are respectively 120 seconds and 30 minutes, with a ten-second connect timeout. Audio within 49 MiB uses sendAudio; larger audio uses documents or splitting. Oversized video/audio is split by estimated duration with stream copy; actual chunk sizes are validated and an oversized chunk fails rather than recursively splitting.

Source URLs remain in captions; YouTube audio also uses channel metadata when available. Working paths use UUIDs, and normal completion paths remove downloaded/prepared files and working directories. Cleanup is best-effort, not transactional delivery or crash recovery.

## Independent YouTube subtitles

List native manual and automatic tracks, excluding live chat and translated `tlang` URLs. Prefer manual tracks over automatic aliases and prefer `-orig` for duplicate native automatic tracks. Paginate eight tracks per page and retain the menu so another language or media can be fetched independently.

Download exactly the chosen language/type with media download disabled and convert it to SRT. Use isolated per-attempt directories and accept exactly one nonempty, regular SRT within that directory; reject ambiguous or escaped output. Discovery and download have 90- and 180-second deadlines per attempt. Deliver a separate document with a sanitized language-based filename, enforce the endpoint's upload limit, and clean its working directory after success or failure. Subtitle errors exposed to chat omit raw yt-dlp metadata, stderr, and signed URLs.

## Automatic cookies and bounded retry

Cookie lookup prefers `cookies.txt` in the working directory, then beside the executable; without a jar, normal cookie-using yt-dlp paths select Brave when its profile directory exists, otherwise Chrome. Single-photo metadata probing uses an available jar but does not require a local browser. Cookie-using commands remove inherited PM2 Node IPC environment variables before running Node-enabled yt-dlp. X commands do not use cookies or cookie-error recovery.

Automatic refresh is disabled without `COOKIE_REFRESH_URL`. Its HTTP URL must use a loopback host with no credentials/query/fragment; proxies and redirects are disabled and `localhost` is pinned to IPv4 loopback. Read the token from its bounded file, mark Authorization sensitive, require HTTP 200, and bound the response to 2 MiB and ten seconds. Never log the token, body, or detailed refresh transport errors.

One async mutex serializes refreshes across callers. A success is cached for ten minutes, a failure backs off for one minute, and failure preserves the previous jar. Before cache reuse, require unexpired local Instagram and YouTube/Google login cookies; missing/expired credentials enter the bounded forced-refresh path. Forced attempts are limited to one per minute, including cancellation/failure; a refresh completed after a failed operation began can serve concurrent failures without another request.

Validate Netscape syntax, scope flags, relevant domains, and both nonempty login-cookie classes before installation. This response validation is not a platform-authentication or expiry-validity check. Preserve validated unrelated-provider rows in the existing jar, replace old Instagram/YouTube/Google rows, and atomically rename a synchronized mode-600 sibling file on Unix. A malformed existing jar can also block replacement; rejection leaves it unchanged.

After an actual unsuccessful cookie-using yt-dlp exit, retry the same operation at most once and only after a successful forced refresh. Spawn/read/wait errors, timeouts, FFmpeg failures, and Telegram upload failures do not trigger that retry. Progress subprocesses are killed on drop; the stderr task is drained briefly or aborted before retry so stale progress cannot edit the next attempt's status.

## Diagnostics and known limitations

Append timestamp, media kind, Telegram user ID, and submitted URL to local `download_links.log` for the confirmed troubleshooting purpose. Optional alerts include failure context and source URL. There is no implemented retention/rotation or general URL redaction policy; ordinary yt-dlp output is logged, and the alert endpoint itself may appear in logs. The refresh client's restricted logging does not imply that all bot diagnostics are sanitized.

Startup performs a real YouTube cookie check before Telegram dispatch; a failed check is logged/optionally alerted but does not itself abort startup. **Confirmed limitation:** ordinary metadata/download commands, startup probing, and FFmpeg work have no overall deadlines, although some specific operations do. The active-job cap does not bound disk use, waiting work, or total runtime.

**Unconfirmed limitation acceptance:** platform behavior/session validity still needs live verification; live extraction/bridge tests are ignored by default. Fixture tests do not establish that real links currently work.

**Unconfirmed limitation acceptance:** delivery can partially succeed, duration-based splitting can reject an oversized result, and some failure/cancellation paths can leave temporary artifacts (for example, an early audio-chunk upload failure exits before chunk cleanup). These observations are not accepted reliability guarantees or newly scheduled work.

## Verification boundary

Rust unit/local-HTTP tests cover routing, menus, queue admission, formats, refresh validation/coalescing/retry, and subtitle selection/output. FFmpeg status/conversion tests are ignored by default but can run locally without Telegram. Live tests require separate explicit selection and runtime prerequisites; never start a real bot poller merely to validate this spec.
