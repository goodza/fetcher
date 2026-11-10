---
id: fetcher-cookie-bridge
type: module-design
status: active
title: Private Firefox cookie export bridge
parent: fetcher-architecture
---

# Private Firefox cookie bridge

## Responsibility and boundary

Export existing Firefox sessions from the operator's PC as Netscape cookies, without logging in, renewing a session, or controlling downloads. The Python standard library owns the HTTP service, SQLite snapshot, and manual refresh client. Runtime topology and the direction of callers live in `fetcher-architecture`; automatic refresh policy is owned by `fetcher-bot`.

The service binds only `127.0.0.1` (default port 19121). Deployment uses an encrypted, shared-key FRP STCP tunnel with loopback listeners, not a new public port. Keep its bearer token, tunnel configuration, cookie jars, and browser profile outside Git; use mode-600 token/config files as specified by the deployment docs. The service validates token contents, not token-file permissions. Do not change unrelated FRPC clients or FRPS port policy to expose this endpoint.

## Export contract

Snapshot `cookies.sqlite` together with its live WAL; an immutable database read would miss live WAL changes, and Firefox may hold an exclusive lock. Compare size/mtime fingerprints around copying, retry up to four times, and reject a persistently changing source. This is a bounded snapshot heuristic, not an asserted transaction across Firefox and the filesystem.

Query only empty `originAttributes` so container and partitioned sessions are not mixed. Keep unexpired/session cookies from Instagram and YouTube host families plus `google.com` and `accounts.google.com`; exclude other Google applications and YouTube `ST-*` header bloat. For duplicate host/path/name rows, the most recently accessed row wins. Reject malformed exported fields and require nonempty Instagram `sessionid` and YouTube/Google `SID`, `__Secure-1PSID`, or `__Secure-3PSID` credentials.

Return UTF-8 Netscape text with a final newline, seven tab-separated fields per row, correct domain-scope flags and HttpOnly prefixes, bounded to 2 MiB. Never return a partial export missing either login class. A successful export proves only that local credentials were copied, not that either platform accepts them; the operator must sign in again when a session is invalidated.

## HTTP contract

Accept an empty authenticated `POST /refresh` with `Authorization: Bearer <token>`. Compare the bearer value in constant time. Service startup requires at least 32 non-whitespace ASCII token characters.

Return 200 for a complete export, 401 for failed authorization, 404 for a different authenticated POST path, 413 for a request body/transfer encoding, 405 for GET, and 503 when export fails. Responses include Content-Length, `Cache-Control: no-store`, and connection close; per-connection socket timeout is five seconds. The standard HTTPServer handles requests serially, not as an unbounded worker pool.

Suppress arbitrary HTTP request logging and do not log headers, cookie values, or response bodies. Log only fixed diagnostics and successful export size. Private transport plus a bearer token is the intended protection; loopback binding alone does not authorize arbitrary local processes.

## Manual refresh client

The one-shot client permits only HTTP to `127.0.0.1` at `/refresh` with no URL credentials/query/fragment, disables proxies/redirects, and uses a ten-second timeout. Require HTTP 200, exactly one numeric Content-Length, an exact body-length match, the size bound, and valid complete Netscape content before replacing the destination.

Write a mode-600 temporary sibling, flush/synchronize it, then atomically replace the destination and remove temporary residue. Failure leaves the old jar unchanged. Unlike the automatic Rust client, manual replacement does not merge unrelated-provider cookies; it installs the bridge export wholesale. The validators also differ: the exporter/manual client narrow Google hosts to the two named hosts, while the Rust client accepts the Google host family. Do not assume these implementations are identical.

## Verification boundary

Python tests use temporary SQLite WAL databases and local HTTP servers to check filtering, missing credentials, malformed rows, authorization, response completeness, atomic replacement, and old-file preservation. No browser session, production token, or live platform request is needed. Runtime tunnel availability and platform session acceptance are separate operational checks, not conclusions of these tests.
