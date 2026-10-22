#!/usr/bin/env python3
"""Loopback-only Firefox -> fetcher cookie webhook. Python stdlib only."""
import argparse
import hmac
import logging
import pathlib
import shutil
import sqlite3
import tempfile
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

LOG = logging.getLogger('fetcher-cookie-bridge')
AUTH_NAMES = {'SID', '__Secure-1PSID', '__Secure-3PSID'}


class ExportError(Exception):
    pass


def allowed_host(host):
    host = host.lstrip('.').lower()
    return (host == 'instagram.com' or host.endswith('.instagram.com')
            or host == 'youtube.com' or host.endswith('.youtube.com')
            or host in {'google.com', 'accounts.google.com'})


def fingerprint(db):
    return [(p.stat().st_size, p.stat().st_mtime_ns) if p.exists() else None
            for p in [db, pathlib.Path(str(db) + '-wal')]]


def export_cookies(profile):
    db = pathlib.Path(profile) / 'cookies.sqlite'
    # Firefox can hold an exclusive SQLite lock. Snapshot DB + WAL, never
    # immutable=1 (which ignores live WAL), never mix container/partition jars.
    with tempfile.TemporaryDirectory(prefix='fetcher-cookies-') as temporary:
        copy = pathlib.Path(temporary) / 'cookies.sqlite'
        for attempt in range(4):
            before = fingerprint(db)
            for suffix in ['', '-wal']:
                source = pathlib.Path(str(db) + suffix)
                target = pathlib.Path(str(copy) + suffix)
                if source.exists():
                    shutil.copyfile(source, target)
                else:
                    target.unlink(missing_ok=True)
            if before == fingerprint(db):
                break
            time.sleep(0.05)
        else:
            raise ExportError('Firefox database remained busy')
        connection = sqlite3.connect(copy, timeout=2)
        try:
            rows = connection.execute('SELECT host,path,isSecure,expiry,name,value,isHttpOnly '
                                      'FROM moz_cookies WHERE originAttributes = ? '
                                      'ORDER BY lastAccessed ASC', ('',)).fetchall()
        finally:
            connection.close()
    now = int(time.time())
    selected = {}
    for host, path, secure, expiry, name, value, http_only in rows:
        if not allowed_host(host) or (expiry != 0 and expiry <= now):
            continue
        if host.lstrip('.').endswith('youtube.com') and name.startswith('ST-'):
            continue
        fields = [host, path, name, value]
        if any(any(ch in field for ch in '\t\r\n\x00') for field in fields):
            raise ExportError('Invalid cookie row')
        if not name or not path.startswith('/'):
            raise ExportError('Invalid cookie row')
        selected[(host, path, name)] = (host, path, secure, expiry, name, value, http_only)
    cookies = list(selected.values())
    if not any(h.lstrip('.').endswith('instagram.com') and n == 'sessionid' and v
               for h, p, s, e, n, v, o in cookies):
        raise ExportError('Instagram login cookie is missing')
    if not any(h.lstrip('.').endswith(('youtube.com', 'google.com')) and n in AUTH_NAMES and v
               for h, p, s, e, n, v, o in cookies):
        raise ExportError('YouTube login cookie is missing')
    lines = ['# Netscape HTTP Cookie File', '# Exported for fetcher; keep private.']
    for host, path, secure, expiry, name, value, http_only in sorted(cookies):
        lines.append('\t'.join([('#HttpOnly_' if http_only else '') + host,
                                'TRUE' if host.startswith('.') else 'FALSE', path,
                                'TRUE' if secure else 'FALSE', str(expiry), name, value]))
    result = ('\n'.join(lines) + '\n').encode()
    if len(result) > 2 * 1024 * 1024:
        raise ExportError('Cookie export is too large')
    return result


def make_server(profile, token, port=19121):
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, format, *args):
            pass  # Never log arbitrary request headers/paths or cookie values.

        def setup(self):
            super().setup()
            self.connection.settimeout(5)

        def reply(self, code, body, content_type='text/plain; charset=utf-8'):
            self.send_response(code)
            self.send_header('Content-Type', content_type)
            self.send_header('Content-Length', str(len(body)))
            self.send_header('Cache-Control', 'no-store')
            self.send_header('Connection', 'close')
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            self.reply(405, b'Use authenticated POST /refresh\n')

        def do_POST(self):
            if not hmac.compare_digest(self.headers.get('Authorization', '').encode(),
                                       ('Bearer ' + token).encode()):
                self.reply(401, b'Unauthorized\n')
                return
            if self.path != '/refresh':
                self.reply(404, b'Not found\n')
                return
            if self.headers.get('Transfer-Encoding') or self.headers.get('Content-Length', '0') != '0':
                self.reply(413, b'Request body must be empty\n')
                return
            try:
                body = export_cookies(profile)
            except Exception:
                LOG.warning('Cookie export failed; existing server cookies remain unchanged')
                self.reply(503, b'Cookie export unavailable\n')
                return
            self.reply(200, body)
            LOG.info('Cookie export delivered (%d bytes)', len(body))

    return HTTPServer(('127.0.0.1', port), Handler)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--profile', type=pathlib.Path, required=True)
    parser.add_argument('--token-file', type=pathlib.Path, required=True)
    parser.add_argument('--port', type=int, default=19121)
    args = parser.parse_args()
    token = args.token_file.read_text().strip()
    if len(token) < 32 or not token.isascii() or any(c.isspace() for c in token):
        parser.error('Token file must contain at least 32 non-whitespace ASCII characters')
    logging.basicConfig(level=logging.INFO, format='%(asctime)s %(levelname)s %(message)s')
    server = make_server(args.profile, token, args.port)
    LOG.info('Listening on 127.0.0.1:%d', args.port)
    server.serve_forever()


if __name__ == '__main__':
    main()
