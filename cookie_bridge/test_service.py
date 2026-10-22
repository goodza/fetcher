import importlib.util
import pathlib
import sqlite3
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request

spec = importlib.util.spec_from_file_location('service', pathlib.Path(__file__).with_name('service.py'))
assert spec is not None and spec.loader is not None
service = importlib.util.module_from_spec(spec)
spec.loader.exec_module(service)


class BridgeTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='fetcher-test-')
        self.profile = pathlib.Path(self.tmp.name)
        self.db = sqlite3.connect(self.profile / 'cookies.sqlite')
        self.db.execute('PRAGMA journal_mode=WAL')
        self.db.execute('CREATE TABLE moz_cookies (host TEXT,path TEXT,isSecure INTEGER,expiry INTEGER,name TEXT,value TEXT,isHttpOnly INTEGER,originAttributes TEXT,lastAccessed INTEGER)')
        self.add('.instagram.com', 'sessionid', 'ig-test')
        self.add('.youtube.com', 'SID', 'yt-test')
        self.add('.google.com', '__Secure-1PSID', 'g-test')

    def tearDown(self):
        self.db.close()
        self.tmp.cleanup()

    def add(self, host, name, value, attrs='', expiry=None):
        self.db.execute('INSERT INTO moz_cookies VALUES (?,?,?,?,?,?,?,?,?)', (host, '/', 1, int(time.time()) + 3600 if expiry is None else expiry, name, value, 1, attrs, int(time.time())))
        self.db.commit()

    def test_export_live_wal_filters_and_no_secrets_outside_scope(self):
        self.add('.youtube.com', 'ST-bloat', 'x' * 10000)
        self.add('.youtube.com', 'expired', 'old', expiry=1)
        self.add('.youtube.com', 'partition', 'private', attrs='^partitionKey=x')
        self.add('.youtube.com', 'container', 'private', attrs='^userContextId=1')
        self.add('.evilinstagram.com', 'no', 'secret')
        self.add('mail.google.com', 'gmail', 'secret')
        result = service.export_cookies(self.profile).decode()
        self.assertTrue(result.startswith('# Netscape HTTP Cookie File\n'))
        self.assertEqual(len([line for line in result.splitlines() if '\t' in line]), 3)
        for value in ['ST-bloat', 'expired', 'partition', 'container', 'secret']:
            self.assertNotIn(value, result)

    def test_invalid_row_is_rejected(self):
        self.add('.instagram.com', 'bad', 'a\nb')
        with self.assertRaises(service.ExportError):
            service.export_cookies(self.profile)

    def test_missing_auth_does_not_return_partial_export(self):
        self.db.execute("DELETE FROM moz_cookies WHERE name='sessionid'")
        self.db.commit()
        with self.assertRaises(service.ExportError):
            service.export_cookies(self.profile)

    def test_client_atomic_update_and_failure_preserves_file(self):
        client_spec = importlib.util.spec_from_file_location('refresh', pathlib.Path(__file__).with_name('refresh.py'))
        assert client_spec is not None and client_spec.loader is not None
        client = importlib.util.module_from_spec(client_spec)
        client_spec.loader.exec_module(client)
        token = self.profile / 'token'
        token.write_text('a' * 64)
        output = self.profile / 'cookies.txt'
        output.write_bytes(b'original')
        server = service.make_server(self.profile, 'a' * 64, port=0)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        url = 'http://127.0.0.1:' + str(server.server_port) + '/refresh'
        try:
            client.refresh(url, token, output)
            good = output.read_bytes()
            self.assertTrue(good.startswith(b'# Netscape'))
            self.assertEqual(output.stat().st_mode & 0o777, 0o600)
            token.write_text('wrong')
            with self.assertRaises(urllib.error.HTTPError) as error:
                client.refresh(url, token, output)
            error.exception.close()
            self.assertEqual(output.read_bytes(), good)
            self.assertFalse(list(self.profile.glob('.cookies-refresh-*')))
            with self.assertRaises(ValueError):
                client.refresh('http://example.com/refresh', token, output)
        finally:
            server.shutdown()
            server.server_close()
            thread.join()

    def test_client_rejects_truncated_partial_and_bad_scope_exports(self):
        import socket
        client_spec = importlib.util.spec_from_file_location('refresh', pathlib.Path(__file__).with_name('refresh.py'))
        assert client_spec is not None and client_spec.loader is not None
        client = importlib.util.module_from_spec(client_spec)
        client_spec.loader.exec_module(client)
        token = self.profile / 'token'
        token.write_text('a' * 64)
        output = self.profile / 'cookies.txt'
        good = service.export_cookies(self.profile)
        cases = [
            (200, good, len(good) + 100),
            (206, good, len(good)),
            (200, good.replace(b'.instagram.com\tTRUE', b'.instagram.com\tFALSE'), None),
            (200, good.rstrip(b'\n'), None),
        ]
        for status, body, length in cases:
            with self.subTest(status=status, length=length):
                output.write_bytes(b'KNOWN_GOOD')
                output.chmod(0o600)
                listener = socket.socket()
                listener.bind(('127.0.0.1', 0))
                listener.listen(1)
                port = listener.getsockname()[1]
                def serve():
                    with listener:
                        connection, _ = listener.accept()
                        with connection:
                            connection.recv(8192)
                            header = f'HTTP/1.1 {status} Test\r\nContent-Length: {len(body) if length is None else length}\r\nConnection: close\r\n\r\n'.encode()
                            connection.sendall(header + body)
                thread = threading.Thread(target=serve, daemon=True)
                thread.start()
                try:
                    with self.assertRaises((ValueError, OSError)):
                        client.refresh(f'http://127.0.0.1:{port}/refresh', token, output)
                    self.assertEqual(output.read_bytes(), b'KNOWN_GOOD')
                    self.assertEqual(output.stat().st_mode & 0o777, 0o600)
                    self.assertFalse(list(self.profile.glob('.cookies-refresh-*')))
                finally:
                    thread.join(timeout=3)

    def test_http_auth_and_routes(self):
        server = service.make_server(self.profile, 'a' * 64, port=0)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        base = 'http://127.0.0.1:' + str(server.server_port)
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        try:
            for path, method, token, expected in [('/refresh', 'POST', '', 401), ('/refresh', 'GET', 'a' * 64, 405), ('/wrong', 'POST', 'a' * 64, 404)]:
                request = urllib.request.Request(base + path, method=method, headers={'Authorization': 'Bearer ' + token})
                with self.assertRaises(urllib.error.HTTPError) as raised:
                    opener.open(request, timeout=3)
                self.assertEqual(raised.exception.code, expected)
                raised.exception.close()
            request = urllib.request.Request(base + '/refresh', data=b'', headers={'Authorization': 'Bearer ' + 'a' * 64})
            with opener.open(request, timeout=3) as response:
                self.assertEqual(response.status, 200)
                self.assertEqual(response.headers['Cache-Control'], 'no-store')
                self.assertTrue(response.read().startswith(b'# Netscape'))
        finally:
            server.shutdown()
            server.server_close()
            thread.join()


if __name__ == '__main__':
    unittest.main()
