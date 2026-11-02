# Fetcher 0.1.4

Rust Telegram media downloader. Requires Rust 1.96 or newer for the locked
dependencies (`rustup update stable` to update). Run `cargo test --locked`, then
`cargo build --release --locked`. Pushing `main` runs the gsweb deployment workflow
and restarts the production bot.

## Firefox cookie bridge

The PC runs a dependency-free Python webhook under PM2. It snapshots the live
Firefox SQLite DB **with WAL**, then exports Netscape cookies for Instagram,
YouTube and Google authentication. Other Google apps, expired cookies,
Firefox containers/partitioned cookies and YouTube `ST-*` header bloat are excluded.
This copies existing sessions; it does not log in or renew an expired session.
Sign in again in Firefox when a platform invalidates its session.

Topology:

```
fetcher on gsweb -> 127.0.0.1:19121/refresh (FRPC STCP visitor)
                -> existing FRPS :7000 -> FRPC on PC
                -> 127.0.0.1:19121 (Python webhook) -> Firefox DB + WAL
```

No new public port. STCP uses a private shared key and encrypted transport;
the webhook additionally requires a random Bearer token from a mode-600 file.
Both HTTP listeners bind loopback. Requests/responses and cookie values must
never be logged, checked into Git, or pasted into chat.

### PC service

```sh
python3 cookie_bridge/service.py \
  --profile "$HOME/snap/firefox/common/.mozilla/firefox/<profile>" \
  --token-file "$HOME/.config/fetcher-cookie-bridge/webhook.token"
```

Deployed PC PM2 apps: `fetcher-cookie-bridge`, `fetcher-cookie-tunnel`.
PM2 is installed at `~/.local/share/fetcher-pm2/node_modules/pm2/bin/pm2`:

```sh
node ~/.local/share/fetcher-pm2/node_modules/pm2/bin/pm2 status
node ~/.local/share/fetcher-pm2/node_modules/pm2/bin/pm2 logs fetcher-cookie-bridge --lines 20 --nostream
systemctl --user status fetcher-pm2.service
```

`fetcher-pm2.service` resurrects the saved PM2 list at user-manager startup;
user lingering is enabled. Protected runtime configs are in
`~/.config/fetcher-cookie-bridge/` (not in this repository).
`pc-frpc.toml` connects to the existing FRPS and defines an STCP proxy named
`fetcher-firefox-cookies`, pointing to local port 19121. The remote visitor uses
that exact server name and STCP key, binding only `127.0.0.1:19121`.
Do not change existing FRPC clients or FRPS allowPorts for this tunnel.

### gsweb configuration

Set these in `~/fetcher/.env`, retaining all existing entries:

```dotenv
COOKIE_REFRESH_URL=http://127.0.0.1:19121/refresh
COOKIE_REFRESH_TOKEN_FILE=/home/edzlf/fetcher/.cookie-refresh-token
```

Fetcher refreshes before cookie-using work and its startup cookie check.
Concurrent requests share one refresh; successful refreshes are cached for ten
minutes, failures back off for one minute. Refresh failures retain the last good
`cookies.txt`, so the PC being offline does not immediately break downloads.
The response is bounded and validated before an atomic mode-600 replacement.
Before reusing that ten-minute cache, fetcher checks that the local jar still
contains unexpired Instagram and YouTube/Google login cookies. If yt-dlp has
removed rejected credentials, the next operation requests fresh PC cookies through
the same tunnel immediately. This recovery shares the forced-refresh limit of
one attempt per minute, so repeated rejection cannot hammer the PC webhook.

If a cookie-using yt-dlp operation exits with an error, fetcher requests a forced
refresh, bypassing the ten-minute success cache, and retries that operation
**once** after a successful refresh. This covers downloads, metadata, startup
checks, and YouTube subtitle discovery/download. Concurrent failures reuse a
refresh completed after their first attempt began. Forced refreshes are limited
to one per minute, including persistent extraction failures; unavailable PC or
rejected cookies preserve the old jar. X downloads remain cookie-free. Process
startup, FFmpeg preparation, and Telegram upload errors do not trigger refresh.
A second yt-dlp failure is returned normally: no infinite retry loop.

Unset `COOKIE_REFRESH_URL` to disable automatic refresh entirely.

The gsweb PM2 app `fetcher-cookie-tunnel` runs the STCP visitor with protected
config `~/.config/fetcher-cookie-bridge/visitor.toml`; it is included in `pm2 save`.
The bot retains its existing PM2 app name `fetcher`.

Manual forced refresh (does not restart the bot):

```sh
ssh gsweb 'python3 ~/fetcher/cookie_bridge/refresh.py'
```

Failure preserves the old file. A successful export proves transport/file
replacement, **not** that a platform accepts the session: verify a real media
request separately. Repeated login/challenge/network errors are not necessarily
stale cookies; do not hammer the webhook to solve unrelated extraction failures.

### Tests

```sh
python3 -m unittest discover -s cookie_bridge -v
cargo fmt --check
cargo test --locked
cargo check --locked
```

Python tests cover live-WAL export, domain/expiry/container filtering, malformed
rows, missing auth cookies, HTTP authorization, truncated/partial responses,
Netscape scope flags, and atomic client replacement with old-file preservation
on failure. Rust tests exercise the bot's refresh client and integration. Tests use isolated fake cookie fixtures, not a live bot
poller. Runtime credentials, cookies and generated PM2/FRPC configs stay outside
Git.
