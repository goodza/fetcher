# Fetcher 0.1.3

Rust Telegram media downloader. Run `cargo test --locked`, then
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
Concurrent requests share one refresh; successful refreshes are cached for five
minutes, failures back off for one minute. Refresh failures retain the last good
`cookies.txt`, so the PC being offline does not immediately break downloads.
The response is bounded and validated before an atomic mode-600 replacement.
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
