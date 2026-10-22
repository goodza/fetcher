#!/usr/bin/env python3
"""One-shot gsweb refresh; same private webhook used automatically by fetcher."""
import argparse
import os
import pathlib
import tempfile
import urllib.parse
import urllib.request

LIMIT = 2 * 1024 * 1024


def validate(body):
    text = body.decode('utf-8')
    if not text.startswith('# Netscape HTTP Cookie File\n') or not text.endswith('\n') or '\r' in text or '\0' in text:
        raise ValueError('Invalid export')
    ig = yt = False
    for line in text.splitlines():
        if not line or (line.startswith('#') and not line.startswith('#HttpOnly_')):
            continue
        parts = line.removeprefix('#HttpOnly_').split('\t')
        if len(parts) != 7:
            raise ValueError('Invalid row')
        host, include, path, secure, expiry, name, value = parts
        if (include == 'TRUE') != host.startswith('.'):
            raise ValueError('Invalid domain scope')
        host = host.lstrip('.')
        allowed = (host == 'instagram.com' or host.endswith('.instagram.com')
                   or host == 'youtube.com' or host.endswith('.youtube.com')
                   or host in {'google.com', 'accounts.google.com'})
        if not allowed or include not in {'TRUE', 'FALSE'} or secure not in {'TRUE', 'FALSE'} or not expiry.isdigit() or not path.startswith('/'):
            raise ValueError('Invalid row')
        ig |= host.endswith('instagram.com') and name == 'sessionid' and bool(value)
        yt |= host.endswith(('youtube.com', 'google.com')) and name in {'SID', '__Secure-1PSID', '__Secure-3PSID'} and bool(value)
    if not (ig and yt):
        raise ValueError('Missing login cookies')


def refresh(url, token_file, output):
    target = urllib.parse.urlsplit(url)
    if (target.scheme != 'http' or target.hostname != '127.0.0.1'
            or target.username or target.password or target.path != '/refresh'
            or target.query or target.fragment):
        raise ValueError('Only loopback FRP webhook is allowed')
    token = pathlib.Path(token_file).read_text().strip()
    request = urllib.request.Request(url, data=b'', headers={'Authorization': 'Bearer ' + token})

    class NoRedirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, *args, **kwargs):
            return None

    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    with opener.open(request, timeout=10) as response:
        lengths = response.headers.get_all('Content-Length', [])
        if response.status != 200 or len(lengths) != 1 or not lengths[0].isdigit():
            raise ValueError('Incomplete export response')
        expected = int(lengths[0])
        if expected > LIMIT:
            raise ValueError('Export too large')
        body = response.read(LIMIT + 1)
        if len(body) != expected:
            raise ValueError('Truncated export response')
    if len(body) > LIMIT:
        raise ValueError('Export too large')
    validate(body)
    output = pathlib.Path(output)
    fd, temporary = tempfile.mkstemp(prefix='.cookies-refresh-', dir=output.parent)
    try:
        with os.fdopen(fd, 'wb') as stream:
            stream.write(body)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, output)
    finally:
        pathlib.Path(temporary).unlink(missing_ok=True)
    return len(body)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--url', default='http://127.0.0.1:19121/refresh')
    parser.add_argument('--token-file', type=pathlib.Path, default=pathlib.Path.home() / 'fetcher/.cookie-refresh-token')
    parser.add_argument('--output', type=pathlib.Path, default=pathlib.Path.home() / 'fetcher/cookies.txt')
    args = parser.parse_args()
    try:
        count = refresh(args.url, args.token_file, args.output)
    except Exception:
        parser.exit(1, 'Refresh failed; existing cookies preserved. Check PC service/tunnel/Firefox login.\n')
    print('Cookies refreshed atomically:', count, 'bytes; mode 600')


if __name__ == '__main__':
    main()
