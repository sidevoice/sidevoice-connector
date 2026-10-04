"""Exercise a selected Rust Connector + native Core pair on macOS with a disposable room.

The room only implements the pairing endpoint and the Core's outbound Socket.IO handshake. The
test installs a target SEA into a fresh profile, calls voice_pair through the Rust MCP process, and
waits until the already-running native Core dials the room using the credential that pairing wrote.
"""

import json
import os
from pathlib import Path
import select
import socket
import subprocess
import tempfile
import threading
import time

import socketio
import uvicorn


CODE = 'TEST-CODE-7'
CONNECTOR_ID = 'connector-pairing-regression'
TOKEN = 'test-only-credential'


def wait_until(check, timeout=25.0, message='timed out'):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        time.sleep(0.05)
    raise AssertionError(message)


async def response(send, status, body):
    encoded = json.dumps(body, separators=(',', ':')).encode()
    await send({'type': 'http.response.start', 'status': status,
                'headers': [(b'content-type', b'application/json'), (b'content-length', str(len(encoded)).encode())]})
    await send({'type': 'http.response.body', 'body': encoded})


def mcp_request(process, request_id, method, params):
    process.stdin.write(json.dumps({'jsonrpc': '2.0', 'id': request_id, 'method': method, 'params': params}) + '\n')
    process.stdin.flush()
    deadline = time.monotonic() + 25
    while time.monotonic() < deadline:
        ready, _, _ = select.select([process.stdout], [], [], max(0, deadline - time.monotonic()))
        if not ready:
            break
        line = process.stdout.readline()
        if not line:
            break
        answer = json.loads(line)
        if answer.get('id') == request_id:
            return answer
    raise AssertionError(f'Rust MCP did not answer {method}; exit={process.poll()}')


def main():
    if os.uname().sysname != 'Darwin' or os.uname().machine not in ('arm64', 'aarch64'):
        raise SystemExit('installed pairing acceptance requires macOS arm64')
    sea = Path(os.environ['SIDEVOICE_TEST_SEA']).resolve()
    if not sea.is_file() or not os.access(sea, os.X_OK):
        raise AssertionError(f'missing target SEA: {sea}')

    with tempfile.TemporaryDirectory(prefix='sidevoice-rust-pair-') as temporary:
        root = Path(temporary)
        home = root / 'home'
        xdg_data = home / 'xdg-data'
        xdg_config = home / 'xdg-config'
        data = home / '.sidevoice'
        for directory in (home, xdg_data, xdg_config, data):
            directory.mkdir(mode=0o700, parents=True, exist_ok=True)
            directory.chmod(0o700)
        env = dict(os.environ)
        for name in list(env):
            if name.startswith('SIDEVOICE_'):
                env.pop(name)
        env.update({'HOME': str(home), 'XDG_DATA_HOME': str(xdg_data), 'XDG_CONFIG_HOME': str(xdg_config),
                    'SIDEVOICE_DATA_DIR': str(data), 'SIDEVOICE_INSTALL_VERIFY_MS': '120000',
                    'SIDEVOICE_CORE_PORT': '0'})

        pair_request = {}
        auth_file = root / 'room-auth.json'
        sio = socketio.AsyncServer(async_mode='asgi', namespaces=['/nodes'])

        @sio.event(namespace='/nodes')
        async def connect(sid, environ, auth):
            auth = auth if isinstance(auth, dict) else {}
            if (auth.get('connector_id'), auth.get('token'), auth.get('protocol')) != (CONNECTOR_ID, TOKEN, 3):
                return False
            temporary_auth = auth_file.with_suffix('.tmp')
            temporary_auth.write_text(json.dumps(auth), encoding='utf8')
            temporary_auth.replace(auth_file)
            await sio.emit('node.welcome', {'protocol': 3}, to=sid, namespace='/nodes')

        async def http_app(scope, receive, send):
            if scope.get('type') != 'http' or scope.get('path') != '/api/connectors/pair' or scope.get('method') != 'POST':
                await response(send, 404, {'detail': 'not found'})
                return
            body = bytearray()
            while True:
                event = await receive()
                body.extend(event.get('body', b''))
                if len(body) > 16384:
                    await response(send, 413, {'detail': 'request too large'})
                    return
                if not event.get('more_body'):
                    break
            request = json.loads(body)
            pair_request.update(request)
            if request.get('code') != CODE:
                await response(send, 400, {'detail': 'wrong test code'})
                return
            await response(send, 200, {'connector_id': CONNECTOR_ID, 'token': TOKEN, 'protocol': 3,
                                       'dial_key': 'test-only-dial-key'})

        app = socketio.ASGIApp(sio, socketio_path='api/connectors/link', other_asgi_app=http_app)
        with socket.socket() as probe:
            probe.bind(('127.0.0.1', 0))
            port = probe.getsockname()[1]
        room_url = f'http://127.0.0.1:{port}'
        server = uvicorn.Server(uvicorn.Config(app, host='127.0.0.1', port=port, log_level='error'))
        thread = threading.Thread(target=server.run, daemon=True)
        thread.start()
        try:
            wait_until(lambda: server.started, message='test room did not start')
            install = subprocess.run([str(sea), 'install', '--service', '--no-agents', '--json'], env=env,
                                     capture_output=True, text=True, timeout=300)
            if install.returncode:
                raise AssertionError(f'install failed: {install.stdout[-3000:]}\n{install.stderr[-3000:]}')
            installed = json.loads(install.stdout.strip().splitlines()[-1])
            release_root = xdg_data / 'sidevoice'
            release = json.loads((release_root / 'current' / 'release.json').read_text(encoding='utf8'))
            if not installed.get('ok') or release.get('runtime_kind') != 'rust-native-v1':
                raise AssertionError(f'install did not select the Rust pair: {install.stdout[-3000:]}')

            rust_connector = release_root / 'current' / 'dist' / 'sidevoice-rust'
            wait_until(lambda: (data / 'connector.sock').exists() and (data / 'core' / 'core.json').exists(),
                       message='launchd did not start the selected Core and Rust Connector')
            if not rust_connector.is_file():
                raise AssertionError('selected Rust Connector executable is missing')

            connector_log = root / 'rust-mcp.stderr'
            with connector_log.open('w', encoding='utf8') as errors:
                mcp = subprocess.Popen([str(rust_connector), '--installed', 'mcp'], env=env, stdin=subprocess.PIPE,
                                       stdout=subprocess.PIPE, stderr=errors, text=True, bufsize=1)
                try:
                    initialized = mcp_request(mcp, 1, 'initialize', {'protocolVersion': '2025-06-18', 'capabilities': {},
                                                                      'clientInfo': {'name': 'pairing-regression', 'version': '1'}})
                    if 'error' in initialized:
                        raise AssertionError(f'MCP initialization failed: {initialized}')
                    mcp.stdin.write(json.dumps({'jsonrpc': '2.0', 'method': 'notifications/initialized'}) + '\n')
                    mcp.stdin.flush()
                    paired = mcp_request(mcp, 2, 'tools/call', {'name': 'voice_pair',
                        'arguments': {'room': room_url, 'code': CODE}})
                    result = paired.get('result', {})
                    contents = result.get('content', [])
                    if result.get('isError') or not contents or json.loads(contents[0]['text']).get('status') != 'paired':
                        raise AssertionError(f'Rust voice_pair did not return its success result: {paired}')
                    saved = json.loads((data / 'credentials.json').read_text(encoding='utf8'))
                    if (saved.get('url'), saved.get('connector_id'), saved.get('token'), saved.get('protocol')) != (
                            room_url, CONNECTOR_ID, TOKEN, 3):
                        raise AssertionError(f'pairing CLI wrote an unexpected credential: {saved}')
                    if pair_request.get('code') != CODE:
                        raise AssertionError(f'the one-time code changed before the selected CLI received it: {pair_request}')
                    auth = wait_until(lambda: json.loads(auth_file.read_text(encoding='utf8')) if auth_file.exists() else None,
                                      message='the real native Core did not dial out with the new pairing')
                    if (auth.get('connector_id'), auth.get('token'), auth.get('protocol')) != (CONNECTOR_ID, TOKEN, 3):
                        raise AssertionError(f'native Core used different pairing credentials: {auth}')
                    print(json.dumps({'ok': True, 'runtime_kind': 'rust-native-v1', 'core_kind': 'rust-native-v1',
                                      'connector_id': CONNECTOR_ID, 'core_authenticated_with_pairing': True}))
                finally:
                    if mcp.poll() is None:
                        mcp.stdin.close()
                        try:
                            mcp.wait(timeout=5)
                        except subprocess.TimeoutExpired:
                            mcp.terminate()
                            try:
                                mcp.wait(timeout=5)
                            except subprocess.TimeoutExpired:
                                mcp.kill()
                                mcp.wait(timeout=5)
                    errors.flush()

        finally:
            subprocess.run([str(sea), 'uninstall', '--json'], env=env, capture_output=True,
                           text=True, timeout=120, check=False)
            server.should_exit = True
            thread.join(timeout=5)
            if thread.is_alive():
                raise AssertionError('test room did not stop')

if __name__ == '__main__':
    main()
