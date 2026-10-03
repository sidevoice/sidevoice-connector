"""No-model conversation parity checks against the pinned real Core v3."""

import asyncio
import hashlib
import hmac
import http.client
import json
import os
from pathlib import Path
import queue
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlencode

from websockets.asyncio.client import unix_connect


def unix_http(socket_path, method, path, body=None, token=None):
    connection = http.client.HTTPConnection('localhost', timeout=5)
    connection.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    connection.sock.settimeout(5)
    connection.sock.connect(str(socket_path))
    headers = {'Host': 'localhost'}
    if body is not None:
        headers['Content-Type'] = 'application/json'
    if token:
        headers['Authorization'] = 'Bearer ' + token
    connection.request(method, path, json.dumps(body).encode() if body is not None else None, headers)
    response = connection.getresponse()
    payload = response.read()
    connection.close()
    return response.status, json.loads(payload) if payload else None


def core_json(socket_path, method, path, body=None, token=None):
    status, result = unix_http(socket_path, method, path, body, token)
    if status >= 400:
        raise AssertionError(f'{method} {path}: HTTP {status} {result}')
    return result


async def until(predicate, description, seconds=30):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        await asyncio.sleep(.1)
    raise AssertionError(f'timed out: {description}')


async def mcp_request(process, method, params, request_id):
    process.stdin.write((json.dumps({'jsonrpc': '2.0', 'id': request_id, 'method': method, 'params': params}) + '\n').encode())
    await process.stdin.drain()
    while True:
        line = await asyncio.wait_for(process.stdout.readline(), 30)
        if not line:
            raise AssertionError(f'MCP closed while waiting for {method}')
        result = json.loads(line)
        if result.get('id') == request_id:
            if 'error' in result:
                raise AssertionError(f'MCP {method}: {result["error"]}')
            return result['result']


async def tool(process, name, args, request_id, *, meta=None):
    params = {'name': name, 'arguments': args}
    if meta is not None:
        params['_meta'] = meta
    result = await mcp_request(process, 'tools/call', params, request_id)
    if result.get('isError'):
        raise AssertionError(f'{name}: {result}')
    return json.loads(result['content'][0]['text'])


async def tool_failure(process, name, args, request_id):
    result = await mcp_request(process, 'tools/call', {'name': name, 'arguments': args}, request_id)
    assert result.get('isError') is True, result
    return result['content'][0]['text']


async def finish(process):
    if process and process.returncode is None:
        process.terminate()
        try:
            await asyncio.wait_for(process.wait(), 8)
        except asyncio.TimeoutError:
            process.kill()
            await process.wait()


async def start_facade(binary, root, env, client_name, capabilities=None):
    process = await asyncio.create_subprocess_exec(str(binary), '--profile-root', str(root), 'mcp',
        stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE, env=env)
    params = {'protocolVersion': '2025-06-18', 'capabilities': capabilities or {},
              'clientInfo': {'name': client_name, 'version': 'synthetic-no-model-client'}}
    initialized = await mcp_request(process, 'initialize', params, 1)
    assert initialized.get('instructions') and 'Sidevoice connects' in initialized['instructions'], initialized
    assert 'tools' in initialized.get('capabilities', {}), initialized
    process.stdin.write(b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
    await process.stdin.drain()
    listed = await mcp_request(process, 'tools/list', {}, 2)
    assert {entry['name'] for entry in listed['tools']} == {
        'voice_connect', 'voice_pair', 'voice_say', 'voice_disconnect', 'voice_pair_device', 'voice_status'}, listed
    prompts = await mcp_request(process, 'prompts/list', {}, 3)
    assert [item['name'] for item in prompts['prompts']] == ['voice-room'], prompts
    prompt = await mcp_request(process, 'prompts/get', {'name': 'voice-room', 'arguments': {'title': 'fixture'}}, 4)
    assert 'Call voice_status' in prompt['messages'][0]['content']['text'], prompt
    return process, listed


class HttpReceiver(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self):
        self.messages = queue.Queue()
        super().__init__(('127.0.0.1', 0), self.Handler)

    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            length = int(self.headers.get('content-length', '0'))
            self.server.messages.put(json.loads(self.rfile.read(length)))
            self.send_response(204)
            self.end_headers()

        def log_message(self, *_args):
            pass


async def core_text(core_socket, token, session, thread, phrase):
    selected = core_json(core_socket, 'POST', '/api/presentation/select',
        {'session_id':session, 'thread_id':thread}, token)
    message_id = str(uuid.uuid4())
    result = core_json(core_socket, 'POST', '/api/presentation/text', {
        'text': phrase, 'session_id': session, 'thread_id': thread,
        'binding_id': selected['binding']['binding_id'], 'message_id': message_id}, token)
    assert result.get('accepted') is True, result
    return {**result, 'session_id':session, 'message_id':message_id}


def history(core_socket, token, thread):
    return core_json(core_socket, 'GET', '/api/presentation/history?thread_id=' + thread, token=token)['messages']


async def wait_status(core_socket, token, thread, message_id, status, seconds=30):
    return await until(lambda: next((row for row in history(core_socket, token, thread)
        if row.get('id') == message_id and row.get('status') == status), None),
        f'{thread} {message_id} status {status}', seconds)


def participant(core_socket, session, thread):
    response = core_json(core_socket, 'GET', '/api/presentation/participants?session_id=' + session)
    return next((item for item in response['participants'] if item['thread_id'] == thread), None)


async def wait_engine(core_socket, session, thread, model, seconds=10):
    def matching():
        item = participant(core_socket, session, thread)
        return item if item and (item.get('engine') or {}).get('model') == model else None
    return await until(matching, f'{thread} engine {model}', seconds)


async def wait_participant(core_socket, session, thread, *, available):
    def matching():
        item = participant(core_socket, session, thread)
        return item if item and item.get('available') is available else None
    return await until(matching, f'{thread} participant available={available}')


async def wait_working(process, thread, state, request_base):
    async def current():
        status = await tool(process, 'voice_status', {'conversation':thread}, request_base)
        binding = next((item for item in status['connector']['bindings'] if item['client_ref'] == thread), None)
        return status if binding and binding.get('working') is state else None
    return await until_async(current, f'{thread} working={state}')


async def wait_room_working(events, thread, state, seconds=10):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        event = await asyncio.wait_for(events.get(), max(.05, deadline - time.monotonic()))
        if event.get('thread_id') == thread and event.get('working') is state:
            return event
    raise AssertionError(f'timed out: Core reported {thread} working={state}')


async def wait_closed_status(process, thread, request_id):
    async def current():
        status = await tool(process, 'voice_status', {'conversation':thread}, request_id)
        return status if status.get('closed_by_room') is True else None
    return await until_async(current, f'{thread} room-close status')


async def wait_core_connected(process, thread, request_id):
    async def current():
        status = await tool(process, 'voice_status', {'conversation':thread}, request_id)
        return status if status.get('connector', {}).get('connected') is True else None
    return await until_async(current, f'{thread} Core link')


async def wait_refusal_status(process, thread, request_id, refused):
    async def current():
        status = await tool(process, 'voice_status', {'conversation':thread}, request_id)
        present = status.get('connector', {}).get('refused') is not None
        return status if present is refused else None
    return await until_async(current, f'{thread} refusal={refused}')


async def wait_bridge_chat(process, thread, request_id):
    async def current():
        status = await tool(process, 'voice_status', {'conversation':thread}, request_id)
        return status if status.get('card', {}).get('bridge_chat_known') is True else None
    return await until_async(current, f'{thread} authenticated Cursor destination')


async def read_room_events(websocket, events):
    while True:
        frame = json.loads(await websocket.recv())
        if frame.get('type') == 'voice-conversation':
            events.put_nowait(frame.get('data') or {})


async def until_async(predicate, description, seconds=30):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = await predicate()
        if value:
            return value
        await asyncio.sleep(.1)
    raise AssertionError(f'timed out: {description}')


async def status_and_reply(process, thread, joined, input_row, expected_caps, request_id):
    status = await tool(process, 'voice_status', {'conversation': thread}, request_id)
    assert status['joined'] is True and status['capabilities'] == expected_caps, status
    assert status['binding_id'] == joined['binding_id'], status
    assert isinstance(status['room_reachable'], bool) and status['room_reachable'] == status['connector']['connected'], status
    said = await tool(process, 'voice_say', {'session_id': input_row['session_id'],
        'revision': input_row['revision'], 'text': 'A synthetic reply for ' + thread}, request_id + 1)
    assert said.get('text_saved') is True and said.get('status') in {'published', 'queued'}, said
    return said


async def adopted_status(process, thread):
    return await tool(process, 'voice_status', {'conversation':thread}, 41)


def append_claude(transcript, text):
    transcript.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    with transcript.open('a') as output:
        output.write(json.dumps({'type': 'user', 'message': {'role': 'user', 'content': text}}) + '\n')


def append_claude_assistant(transcript, model):
    transcript.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    with transcript.open('a') as output:
        output.write(json.dumps({'type': 'assistant', 'message': {'model': model}}) + '\n')


def set_claude_session_status(record, status):
    value = json.loads(record.read_text())
    value['status'] = status
    record.write_text(json.dumps(value))
    record.chmod(0o600)


def append_cursor(transcript, text):
    transcript.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    with transcript.open('a') as output:
        output.write(json.dumps({'role': 'user', 'message': {'content': [{'type': 'text', 'text': text}]}}) + '\n')


def append_cursor_connect(transcript, title):
    transcript.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    with transcript.open('a') as output:
        output.write(json.dumps({'role':'assistant','message':{'content':[{'type':'tool_use',
            'name':'voice_connect','input':{'title':title}}]}}) + '\n')


def append_cursor_turn_ended(transcript):
    with transcript.open('a') as output:
        output.write(json.dumps({'type':'turn_ended','status':'success'}) + '\n')


def make_cursor_store(config, workspace, chat, model):
    store = config / 'chats' / workspace / chat / 'store.db'
    store.parent.mkdir(parents=True, mode=0o700, exist_ok=True)
    db = sqlite3.connect(store)
    db.execute('CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT)')
    db.execute('INSERT INTO meta(key,value) VALUES(?,?)', ('0', json.dumps({'lastUsedModel':model})))
    db.commit()
    db.close()
    store.chmod(0o600)
    return store


def test_sqlite_cli(root):
    """Cursor ships sqlite3 on macOS; the fixture shim exercises the same read-only query on Linux."""
    binary = root / 'bin' / 'sqlite3'
    binary.write_text('''#!/usr/bin/env python3
import json, sqlite3, sys
args = sys.argv[1:]
if args[:2] != ['-readonly', '-json'] or len(args) != 4:
    raise SystemExit(2)
db = sqlite3.connect('file:' + args[2] + '?mode=ro', uri=True)
db.row_factory = sqlite3.Row
try:
    print(json.dumps([dict(row) for row in db.execute(args[3])]))
finally:
    db.close()
''')
    binary.chmod(0o700)
    return binary


def fake_tools(root, cursor_chat):
    codex = root / 'bin' / 'codex'
    codex.write_text('''#!/usr/bin/env python3
import json, os, pathlib, sys
args = sys.argv[1:]
if args[:2] != ['queue', '--thread'] or args[3] != '--message':
    raise SystemExit(2)
pathlib.Path(os.environ['SIDEVOICE_TEST_CODEX_QUEUE']).write_text(json.dumps({'thread':args[2], 'message':args[4]}))
''')
    codex.chmod(0o700)
    tmux = root / 'bin' / 'tmux-fixture'
    tmux.write_text(f'''#!/usr/bin/env python3
import pathlib, sys
args = sys.argv[1:]
root = pathlib.Path(__file__).resolve().parent.parent
chat = {cursor_chat!r}
if 'list-sessions' in args:
    if (root / 'cursor-persist-enabled').exists():
        print('cursor-fixture\\t1\\t' + chat + '\\t0\\t%1\\t0')
elif 'load-buffer' in args:
    (root / 'cursor-buffer').write_bytes(sys.stdin.buffer.read())
elif 'paste-buffer' in args:
    (root / 'cursor-pasted').write_bytes((root / 'cursor-buffer').read_bytes())
elif 'send-keys' in args and args[-1] == 'Enter':
    (root / 'cursor-sent').write_bytes((root / 'cursor-pasted').read_bytes())
''')
    tmux.chmod(0o700)
    return codex, tmux


async def run_claude_uds(path, token, transcript, received):
    async def handle(reader, writer):
        try:
            auth = json.loads(await asyncio.wait_for(reader.readline(), 10))
            frame = json.loads(await asyncio.wait_for(reader.readline(), 10))
            assert auth == {'type': 'auth', 'token': token}, auth
            assert frame.get('type') == 'user' and frame['message']['role'] == 'user', frame
            received.put_nowait(frame['message']['content'])
            await asyncio.sleep(2)
            append_claude(transcript, frame['message']['content'])
        finally:
            writer.close()
            await writer.wait_closed()

    return await asyncio.start_unix_server(handle, path=str(path))


async def start_bridge(directory, user_data, socket_path, chat, received, transcript):
    async def handle(reader, writer):
        try:
            request = await reader.readuntil(b'\r\n\r\n')
            headers = request.decode(errors='replace').split('\r\n')
            assert any(line.lower() == 'authorization: bearer synthetic-bridge-token' for line in headers), headers
            length = int(next(line.split(':', 1)[1] for line in headers if line.lower().startswith('content-length:')))
            body = json.loads(await reader.readexactly(length))
            if body['type'] == 'listThreads':
                listed = getattr(received, 'listed_chat', chat)
                response = {'threads': [{'id': listed, 'title': 'Synthetic Cursor', 'source': 'local', 'status': 'running'}]}
            elif body['type'] == 'sendMessage':
                received.put_nowait(body)
                response = {'outcome': 'submitted', 'threadTitle': 'Synthetic Cursor'}
            else:
                response = {'outcome': 'error'}
            data = json.dumps(response).encode()
            writer.write(b'HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: '
                + str(len(data)).encode() + b'\r\n\r\n' + data)
            await writer.drain()
        finally:
            writer.close()
            await writer.wait_closed()

    server = await asyncio.start_unix_server(handle, path=str(socket_path))
    directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    bridge_info = directory / 'fixture.json'
    bridge_info.write_text(json.dumps({'protocolVersion': 1, 'pid': os.getpid(), 'socketPath': str(socket_path),
        'token': 'synthetic-bridge-token', 'appName': 'Cursor fixture', 'appVersion': 'fixture',
        'userDataDir': str(user_data), 'createdAt': int(time.time() * 1000)}))
    bridge_info.chmod(0o600)
    return server, bridge_info


async def test_route(binary, root, env, presentation, route, client_name, route_env, *, client_caps=None,
                     thread=None, before=None, after=None, expected_caps=None, expected_initial=None,
                     conversation=None, request_base=10):
    facade, listed = await start_facade(binary, root, {**env, **route_env}, client_name, client_caps)
    try:
        if before:
            await before(facade, listed)
        joined = await tool(facade, 'voice_connect', {'title': 'Synthetic ' + route}, request_base)
        thread = thread or joined['conversation']
        assert joined['conversation'] == thread, joined
        assert joined['status'] in {'joined', 'joining'}, joined
        assert joined['capabilities'] == expected_caps, joined
        assert joined['binding_id'] and not joined['binding_id'].startswith('local-'), joined
        if expected_initial:
            expected_initial(joined)
        input_row = await core_text(presentation['socket'], presentation['token'], presentation['session'],
            thread, 'Core input for ' + route)
        if after:
            await after(facade, joined, input_row, route_env)
        return facade, joined, input_row
    except Exception:
        await finish(facade)
        raise


async def parity_fixture():
    binary = Path(os.environ['SIDEVOICE_RUST_PROOF_BIN']).resolve()
    # Keep the virtualenv path: resolving its symlink bypasses the venv's import path.
    python = Path(os.environ['SIDEVOICE_CORE_PYTHON'])
    with tempfile.TemporaryDirectory(prefix='sidevoice-rust-conversation-') as temporary:
        root = Path(temporary).resolve()
        root.chmod(0o700)
        home, claude_home, codex_home = (root / name for name in ('home', 'claude', 'codex'))
        cursor_root = root / 'cursor'
        cursor_config = cursor_root / 'config'
        cursor_data = cursor_root / 'data'
        xdg_config = root / 'xdg' / 'config'
        xdg_data = root / 'xdg' / 'data'
        data = root / 'sidevoice'
        core_data = data / 'core'
        tool_dir = root / 'bin'
        for directory in (home, claude_home, codex_home, cursor_root, cursor_config, cursor_data,
                root / 'xdg', xdg_config, xdg_data,
                data, core_data, tool_dir):
            directory.mkdir(parents=True, mode=0o700, exist_ok=True)
            directory.chmod(0o700)
        cursor_chat = str(uuid.uuid4())
        test_sqlite_cli(root)
        codex_bin, tmux_bin = fake_tools(root, cursor_chat)
        workspace = 'a' * 32
        store = make_cursor_store(cursor_config, workspace, cursor_chat, 'cursor-cli-fixture-model')
        store_handle = store.open('rb')
        cursor_project = home / '.cursor' / 'projects' / 'synthetic-workspace' / 'agent-transcripts' / cursor_chat
        cursor_transcript = cursor_project / f'{cursor_chat}.jsonl'
        bridge_user_data = root / 'bridge-user-data'
        state_db = bridge_user_data / 'User' / 'globalStorage' / 'state.vscdb'
        state_db.parent.mkdir(parents=True, mode=0o700)
        state = sqlite3.connect(state_db)
        state.execute('CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value TEXT)')
        state.commit()
        state.close()
        bridge_dir = root / 'desktop-bridge'
        bridge_socket = root / 'desktop-bridge.sock'
        claude_session = str(uuid.uuid4())
        claude_token = 'synthetic-claude-token'
        claude_socket = root / 'claude-inbox.sock'
        claude_transcript = claude_home / 'projects' / 'synthetic-workspace' / f'{claude_session}.jsonl'
        sessions = claude_home / 'sessions'
        sessions.mkdir(mode=0o700)
        (sessions / 'allowed.json').write_text(json.dumps({'sessionId': claude_session, 'pid': os.getpid(), 'status': 'idle'}))
        (claude_home / 'settings.json').write_text(json.dumps({'permissions': {'defaultMode': 'default'}}))
        (claude_home / 'settings.json').chmod(0o600)
        received_claude = asyncio.Queue()
        claude_server = await run_claude_uds(claude_socket, claude_token, claude_transcript, received_claude)
        receiver = HttpReceiver()
        receiver_thread = threading.Thread(target=receiver.serve_forever, daemon=True)
        receiver_thread.start()
        codex_thread = str(uuid.uuid4())
        codex_queue = root / 'codex-queue.json'
        codex_sessions = codex_home / 'sessions' / '2026' / '10' / '03'
        codex_sessions.mkdir(parents=True, mode=0o700)
        rollout = codex_sessions / f'fixture-{codex_thread}.jsonl'
        rollout.touch(mode=0o600)
        cursor_buffer = root / 'cursor-buffer'
        cursor_pasted = root / 'cursor-pasted'
        cursor_sent = root / 'cursor-sent'
        adapter_keys = {'CLAUDE_CODE_SESSION_ID', 'CLAUDE_CODE_MESSAGING_SOCKET', 'CLAUDE_CODE_MESSAGING_TOKEN',
            'CODEX_THREAD_ID', 'SIDEVOICE_THREAD', 'SIDEVOICE_DELIVERY_URL', 'SIDEVOICE_HARNESS', 'SIDEVOICE_TITLE'}
        inherited_env = {key: value for key, value in os.environ.items() if key not in adapter_keys}
        base_env = {**inherited_env, 'HOME': str(home), 'CLAUDE_CONFIG_DIR': str(claude_home),
            'CODEX_HOME': str(codex_home), 'CURSOR_CONFIG_DIR': str(cursor_config), 'CURSOR_DATA_DIR': str(cursor_data),
            'XDG_CONFIG_HOME': str(xdg_config), 'XDG_DATA_HOME': str(xdg_data),
            'CURSOR_DESKTOP_BRIDGE_DIR': str(bridge_dir), 'SIDEVOICE_DATA_DIR': str(data),
            'SIDEVOICE_SERVICE_MANAGER': 'none', 'SIDEVOICE_CODEX_BIN': str(codex_bin),
            'SIDEVOICE_CURSOR_LOOKUP_AT': '25,50,75,100', 'SIDEVOICE_CURSOR_SCAN_MS': '10',
            'SIDEVOICE_TMUX_BIN': str(tmux_bin), 'SIDEVOICE_TEST_CODEX_QUEUE': str(codex_queue),
            'PATH': str(tool_dir) + os.pathsep + os.environ.get('PATH', '/usr/bin:/bin')}
        core_log = (root / 'core.log').open('wb')
        launch_id = str(uuid.uuid4())
        core_runner = root / 'core-fixture-runner.py'
        core_runner.write_text('''import sidevoice_core.server.app as app_module
original = app_module.create_app
def create_app(room=None, **kwargs):
    app = original(room, **kwargs)
    @app.post('/api/test/rendezvous-state')
    async def rendezvous_state(payload: dict):
        await room.control.rendezvous_changed(payload)
        return {'sent': True}
    @app.post('/api/test/connector-reconnect')
    async def connector_reconnect():
        peers = list(room.control.peers.values())
        if not peers:
            return {'sent': False}
        await peers[-1].disconnect()
        return {'sent': True}
    return app
app_module.create_app = create_app
from sidevoice_core.server.__main__ import main
main()
''')
        core = await asyncio.create_subprocess_exec(str(python), str(core_runner), '--data-dir', str(core_data),
            '--socket', str(core_data / 'local.sock'), '--port', '0', '--idle-exit', '0', '--launch-id', launch_id,
            '--log-file', str(root / 'core-app.log'), stdout=core_log, stderr=core_log, env=base_env)
        daemon = None
        bridge_server = None
        presentation_reader = None
        room_events = asyncio.Queue()
        facades = []
        held_process = None
        try:
            ready = core_data / 'core.json'
            await until(ready.exists, 'pinned Core ready', 90)
            assert json.loads(ready.read_text())['launch_id'] == launch_id
            daemon = await asyncio.create_subprocess_exec(str(binary), '--profile-root', str(root), 'connector',
                stdout=asyncio.subprocess.DEVNULL, stderr=core_log, env=base_env)
            await until((data / 'connector.sock').exists, 'Rust connector socket')
            token = core_json(core_data / 'local.sock', 'POST', '/api/device/local/pair', {'name': 'rust-conversation-fixture'})['token']
            async with unix_connect(path=str(core_data / 'local.sock'), uri='ws://localhost/api/presentation/ws',
                    subprotocols=['sidevoice', 'sidevoice.token.' + token]) as ws:
                await ws.send(json.dumps({'label': 'rtvi-ai', 'type': 'client-ready', 'id': 'conversation-fixture',
                    'data': {'settings': {'turn_end_mode': 'timer'}}}))
                while True:
                    frame = json.loads(await asyncio.wait_for(ws.recv(), 15))
                    if frame.get('type') == 'voice-session':
                        session = frame['data']['session_id']
                        break
                presentation_reader = asyncio.create_task(read_room_events(ws, room_events))
                presentation = {'socket': core_data / 'local.sock', 'token': token, 'session': session}

                async def claude_after(facade, joined, row, route_env):
                    envelope = await asyncio.wait_for(received_claude.get(), 5)
                    assert 'Core input for Claude' in envelope and '"message_id"' in envelope, envelope
                    set_claude_session_status(sessions / 'allowed.json', 'busy')
                    await wait_room_working(room_events, joined['conversation'], True)
                    await wait_status(core_data / 'local.sock', token, joined['conversation'], row['id'], 'unconfirmed')
                    await wait_status(core_data / 'local.sock', token, joined['conversation'], row['id'], 'read')
                    set_claude_session_status(sessions / 'allowed.json', 'idle')
                    await wait_room_working(room_events, joined['conversation'], False)
                    await status_and_reply(facade, joined['conversation'], joined, row, CLAUDE_CAPS, 40)
                    append_claude_assistant(claude_transcript, 'claude-fixture-model')
                    await wait_engine(core_data / 'local.sock', session, joined['conversation'], 'claude-fixture-model')

                async def codex_after(facade, joined, row, route_env):
                    await wait_engine(core_data / 'local.sock', session, joined['conversation'], 'codex-launch-model')
                    queued = await until(lambda: json.loads(codex_queue.read_text()) if codex_queue.exists() else None, 'Codex fixture queue')
                    assert queued['thread'] == codex_thread and 'Core input for Codex' in queued['message'], queued
                    await wait_status(core_data / 'local.sock', token, joined['conversation'], row['id'], 'delivered')
                    rollout.write_text(json.dumps({'type':'session_meta','payload':{'model':'codex-session-meta-model'}}) + '\n')
                    await wait_engine(core_data / 'local.sock', session, joined['conversation'], 'codex-session-meta-model')
                    with rollout.open('a') as output:
                        output.write(json.dumps({'type':'turn_context','payload':{'model':'codex-fixture-model'}}) + '\n'
                        + json.dumps({'type': 'event_msg', 'payload': {'type': 'task_started', 'turn_id': 'fixture-turn'}}) + '\n'
                        + json.dumps({'type': 'response_item', 'payload': {'type': 'message', 'role': 'user',
                            'content': [{'type': 'input_text', 'text': queued['message']}]}}) + '\n')
                    await wait_engine(core_data / 'local.sock', session, joined['conversation'], 'codex-fixture-model')
                    await wait_working(facade, joined['conversation'], True, 90)
                    await wait_room_working(room_events, joined['conversation'], True)
                    core_json(core_data / 'local.sock', 'POST', '/api/test/connector-reconnect', {})
                    await wait_participant(core_data / 'local.sock', session, joined['conversation'], available=False)
                    while not room_events.empty():
                        room_events.get_nowait()
                    await wait_participant(core_data / 'local.sock', session, joined['conversation'], available=True)
                    await wait_room_working(room_events, joined['conversation'], True)
                    rebound = await tool(facade, 'voice_status', {'conversation':joined['conversation']}, 94)
                    assert rebound['joined'] is True and rebound['binding_id'], rebound
                    joined['binding_id'] = rebound['binding_id']
                    await wait_status(core_data / 'local.sock', token, joined['conversation'], row['id'], 'read')
                    with rollout.open('a') as output:
                        output.write(json.dumps({'type': 'event_msg', 'payload': {'type': 'task_complete', 'turn_id': 'fixture-turn'}}) + '\n')
                    await wait_working(facade, joined['conversation'], False, 92)
                    await wait_room_working(room_events, joined['conversation'], False)
                    await status_and_reply(facade, joined['conversation'], joined, row, CODEX_CAPS, 50)

                async def cursor_cli_after(facade, joined, row, route_env):
                    await wait_engine(core_data / 'local.sock', session, joined['conversation'], 'cursor-cli-fixture-model')
                    pasted = await until(lambda: cursor_sent.read_text() if cursor_sent.exists() else None, 'Cursor persist paste')
                    assert 'Core input for Cursor persist' in pasted, pasted
                    await wait_status(core_data / 'local.sock', token, joined['conversation'], row['id'], 'unconfirmed')
                    append_cursor(cursor_transcript, pasted)
                    await wait_room_working(room_events, joined['conversation'], True)
                    await wait_status(core_data / 'local.sock', token, joined['conversation'], row['id'], 'read')
                    append_cursor_turn_ended(cursor_transcript)
                    await wait_room_working(room_events, joined['conversation'], False)
                    await status_and_reply(facade, joined['conversation'], joined, row, CURSOR_CAPS, 60)

                # Claude Code: inbox delivery is unknown until its transcript proves it was read.
                CLAUDE_CAPS = {'deliver':'supported','inspectInbound':'supported','working':'supported','endOfTurn':'supported','sessionIdentity':'supported'}
                claude_env = {'CLAUDE_CODE_SESSION_ID': claude_session, 'CLAUDE_CODE_MESSAGING_SOCKET': str(claude_socket),
                    'CLAUDE_CODE_MESSAGING_TOKEN': claude_token}
                facade, joined, row = await test_route(binary, root, base_env, presentation, 'Claude', 'claude', claude_env,
                    thread=claude_session, expected_caps=CLAUDE_CAPS, after=claude_after, request_base=10)
                facades.append(facade)
                await tool(facade, 'voice_disconnect', {}, 42)
                rejoined = await tool(facade, 'voice_connect', {'title':'Claude reconnect'}, 43)
                assert rejoined['conversation'] == claude_session and rejoined['capabilities'] == CLAUDE_CAPS, rejoined
                await tool(facade, 'voice_disconnect', {}, 44)
                await finish(facade)
                facades.remove(facade)

                # Codex queue/rollout behavior and truthful capability status.
                CODEX_CAPS = {'deliver':'supported','inspectInbound':'unsupported','working':'supported','endOfTurn':'supported','sessionIdentity':'supported'}
                codex_env = {'CODEX_THREAD_ID': codex_thread, 'SIDEVOICE_TEST_CODEX_QUEUE': str(codex_queue),
                    'CODEX_MODEL':'codex-launch-model'}
                facade, joined, row = await test_route(binary, root, base_env, presentation, 'Codex', 'codex', codex_env,
                    thread=codex_thread, expected_caps=CODEX_CAPS, after=codex_after, request_base=20)
                facades.append(facade)
                assert 'deliver' not in joined.get('experimental', []), joined
                await tool(facade, 'voice_disconnect', {}, 52)
                rejoined = await tool(facade, 'voice_connect', {'title':'Codex reconnect'}, 53)
                assert rejoined['conversation'] == codex_thread and rejoined['capabilities'] == CODEX_CAPS, rejoined
                await tool(facade, 'voice_disconnect', {}, 54)
                await finish(facade)
                facades.remove(facade)

                # Codex can keep its rollout-backed work/read signals while a configured HTTP receiver takes input.
                codex_http_thread = str(uuid.uuid4())
                codex_http_rollout = codex_sessions / f'fixture-http-{codex_http_thread}.jsonl'
                codex_http_rollout.touch(mode=0o600)
                codex_http_env = {'CODEX_THREAD_ID':codex_http_thread,
                    'SIDEVOICE_DELIVERY_URL':f'http://127.0.0.1:{receiver.server_port}/codex-input',
                    'CODEX_MODEL':'codex-http-launch-model'}
                async def codex_http_after(http_facade, http_joined, http_row, _route_env):
                    await wait_engine(core_data / 'local.sock', session, codex_http_thread, 'codex-http-launch-model')
                    request = await asyncio.to_thread(receiver.messages.get, True, 10)
                    assert request['thread_id'] == codex_http_thread and request['text'] == 'Core input for Codex HTTP', request
                    await wait_status(core_data / 'local.sock', token, codex_http_thread, http_row['id'], 'delivered')
                    header = json.dumps({'channel':request['channel'],'session_id':request['session_id'],
                        'revision':request['revision'],'message_id':request['message_id']}, separators=(',', ':'))
                    envelope = header + '\n\n' + request['text']
                    codex_http_rollout.write_text(json.dumps({'type':'event_msg','payload':{'type':'task_started','turn_id':'fixture-http-turn'}}) + '\n'
                        + json.dumps({'type':'response_item','payload':{'type':'message','role':'user',
                            'content':[{'type':'input_text','text':envelope}]}}) + '\n'
                        + json.dumps({'type':'event_msg','payload':{'type':'task_complete','turn_id':'fixture-http-turn'}}) + '\n')
                    await wait_status(core_data / 'local.sock', token, codex_http_thread, http_row['id'], 'read')
                    await status_and_reply(http_facade, codex_http_thread, http_joined, http_row, CODEX_CAPS, 81)
                facade, joined, row = await test_route(binary, root, base_env, presentation, 'Codex HTTP', 'codex', codex_http_env,
                    thread=codex_http_thread, expected_caps=CODEX_CAPS, after=codex_http_after, request_base=80)
                facades.append(facade)
                await tool(facade, 'voice_disconnect', {}, 83)
                rejoined = await tool(facade, 'voice_connect', {'title':'Codex HTTP reconnect'}, 84)
                assert rejoined['conversation'] == codex_http_thread and rejoined['capabilities'] == CODEX_CAPS, rejoined
                await tool(facade, 'voice_disconnect', {}, 85)
                await finish(facade)
                facades.remove(facade)

                # Cursor CLI persist: the parent holds the exact synthetic Cursor chat store open.
                CURSOR_CAPS = {'deliver':'supported','inspectInbound':'unsupported','working':'supported','endOfTurn':'supported','sessionIdentity':'supported'}
                (root / 'cursor-persist-enabled').touch(mode=0o600)
                facade, listed = await start_facade(binary, root, base_env, 'Cursor')
                facades.append(facade)
                joined = await tool(facade, 'voice_connect', {'title':'Synthetic Cursor persist'}, 30)
                assert joined['conversation'] == cursor_chat and joined['capabilities'] == CURSOR_CAPS, joined
                assert joined.get('experimental') == ['deliver'], joined
                input_row = await core_text(core_data / 'local.sock', token, session, cursor_chat, 'Core input for Cursor persist')
                await cursor_cli_after(facade, joined, input_row, {})
                await tool(facade, 'voice_disconnect', {}, 62)
                reconnect = await tool(facade, 'voice_connect', {'title':'Cursor persist reconnect'}, 63)
                assert reconnect['conversation'] == cursor_chat and reconnect['capabilities'] == CURSOR_CAPS, reconnect
                await tool(facade, 'voice_disconnect', {}, 64)
                # The same chat with no persist session reports delivery unsupported; Core does not retry it.
                (root / 'cursor-persist-enabled').unlink()
                unsupported = base_env
                plain, _ = await start_facade(binary, root, unsupported, 'Cursor')
                facades.append(plain)
                held = await tool(plain, 'voice_connect', {'title':'Synthetic Cursor plain CLI'}, 31)
                assert held['conversation'] == cursor_chat and held['capabilities']['deliver'] == 'unsupported', held
                plain_row = await core_text(core_data / 'local.sock', token, session, cursor_chat, 'Held unsupported Cursor message')
                await wait_status(core_data / 'local.sock', token, cursor_chat, plain_row['id'], 'not_sent')
                plain_status = await tool(plain, 'voice_status', {'conversation':cursor_chat}, 33)
                assert plain_status['joined'] is True and plain_status['capabilities']['deliver'] == 'unsupported', plain_status
                typed = held['voice_in']['speak_with']
                typed_reply = await tool(plain, 'voice_say', {'session_id':typed['session_id'], 'revision':typed['revision'],
                    'text':'A synthetic typed-chat reply'}, 34)
                assert typed_reply.get('text_saved') is True and typed_reply.get('status') in {'published','queued'}, typed_reply
                await tool(plain, 'voice_disconnect', {}, 32)
                plain_again = await tool(plain, 'voice_connect', {'title':'Cursor plain reconnect'}, 35)
                assert plain_again['conversation'] == cursor_chat and plain_again['capabilities']['deliver'] == 'unsupported', plain_again
                plain_status = await tool(plain, 'voice_status', {'conversation':cursor_chat}, 36)
                assert plain_status['joined'] is True and plain_status['capabilities']['deliver'] == 'unsupported', plain_status
                await tool(plain, 'voice_disconnect', {}, 37)
                await finish(plain)
                facades.remove(plain)
                await finish(facade)
                facades.remove(facade)

                # Generic HTTP keeps delivery accepted and read unsupported.
                HTTP_CAPS = {'deliver':'supported','inspectInbound':'unsupported','working':'unsupported','endOfTurn':'unsupported','sessionIdentity':'supported'}
                http_thread = str(uuid.uuid4())
                http_env = {'SIDEVOICE_THREAD': http_thread, 'SIDEVOICE_DELIVERY_URL':f'http://127.0.0.1:{receiver.server_port}/input'}
                facade, joined, row = await test_route(binary, root, base_env, presentation, 'HTTP', 'fixture-http', http_env,
                    thread=http_thread, expected_caps=HTTP_CAPS, request_base=70)
                facades.append(facade)
                request = await asyncio.to_thread(receiver.messages.get, True, 10)
                assert request['thread_id'] == http_thread and request['text'] == 'Core input for HTTP', request
                await wait_status(core_data / 'local.sock', token, http_thread, row['id'], 'delivered')
                await status_and_reply(facade, http_thread, joined, row, HTTP_CAPS, 72)
                # A second MCP owner cannot steal the still-registered foreign binding.
                foreign, _ = await start_facade(binary, root, base_env | http_env, 'fixture-http')
                facades.append(foreign)
                refusal = await tool_failure(foreign, 'voice_connect', {'title':'Foreign owner'}, 75)
                assert 'another façade' in refusal, refusal
                await finish(foreign)
                facades.remove(foreign)
                old_binding_id = joined['binding_id']
                await finish(daemon)
                daemon = None
                daemon = await asyncio.create_subprocess_exec(str(binary), '--profile-root', str(root), 'connector',
                    stdout=asyncio.subprocess.DEVNULL, stderr=core_log, env=base_env)
                await until((data / 'connector.sock').exists, 'restarted Rust connector socket')
                rebound = None
                for request_id in range(78, 108):
                    rebound = await tool(facade, 'voice_status', {'conversation':http_thread}, request_id)
                    if rebound['binding_id'] != old_binding_id:
                        break
                    await asyncio.sleep(.1)
                assert rebound['binding_id'] != old_binding_id, rebound
                assert next(item for item in rebound['connector']['bindings']
                    if item['client_ref'] == http_thread)['binding_id'] == rebound['binding_id'], rebound
                restarted_row = await core_text(core_data / 'local.sock', token, session,
                    http_thread, 'Core input after connector restart')
                restarted_request = await asyncio.to_thread(receiver.messages.get, True, 10)
                assert restarted_request['thread_id'] == http_thread and restarted_request['text'] == 'Core input after connector restart', restarted_request
                await wait_status(core_data / 'local.sock', token, http_thread, restarted_row['id'], 'delivered')
                await tool(facade, 'voice_disconnect', {}, 109)
                reconnect = await tool(facade, 'voice_connect', {'title':'HTTP reconnect'}, 110)
                assert reconnect['conversation'] == http_thread and reconnect['capabilities'] == HTTP_CAPS, reconnect
                race_thread = str(uuid.uuid4())
                race_env = {'SIDEVOICE_THREAD':race_thread,
                    'SIDEVOICE_DELIVERY_URL':f'http://127.0.0.1:{receiver.server_port}/state-race'}
                race_peer, _ = await start_facade(binary, root, base_env | race_env, 'fixture-http')
                facades.append(race_peer)
                core_json(core_data / 'local.sock', 'POST', '/api/presentation/select',
                    {'session_id':session,'thread_id':http_thread}, token)
                # A fresh registration and room close overlap as separate Core and MCP clients.
                await asyncio.gather(
                    asyncio.to_thread(core_json, core_data / 'local.sock', 'POST',
                        '/api/presentation/close', {'thread_id':http_thread}, token),
                    tool(race_peer, 'voice_connect', {'title':'Concurrent state writer'}, 124),
                )
                def close_and_guard_persisted():
                    if not (data / 'conversation-state.json').exists():
                        return None
                    state = json.loads((data / 'conversation-state.json').read_text())
                    if state.get('closed_by_room', {}).get(http_thread) and (data / 'conversation-state.guard').read_bytes()[8] == 0:
                        return state
                    return None
                await until(close_and_guard_persisted, 'Rust records room closure and clears its guard')
                await finish(daemon)
                daemon = None
                daemon = await asyncio.create_subprocess_exec(str(binary), '--profile-root', str(root), 'connector',
                    stdout=asyncio.subprocess.DEVNULL, stderr=core_log, env=base_env)
                await until((data / 'connector.sock').exists, 'restarted Rust connector after room closure')
                closed_status = await wait_closed_status(facade, http_thread, 112)
                assert closed_status['joined'] is False and closed_status.get('closed_by_room') is True, closed_status
                closed_say = await tool_failure(facade, 'voice_say', {'conversation':http_thread,
                    'session_id':'typed:' + http_thread, 'revision':0, 'text':'Must not be published'}, 113)
                assert 'closed this conversation' in closed_say, closed_say
                await tool(race_peer, 'voice_disconnect', {}, 126)
                await finish(race_peer)
                facades.remove(race_peer)
                reopened = await tool(facade, 'voice_connect', {'title':'HTTP re-enabled after room close'}, 114)
                assert reopened['conversation'] == http_thread, reopened
                await wait_core_connected(facade, http_thread, 121)
                core_json(core_data / 'local.sock', 'POST', '/api/test/rendezvous-state',
                    {'connected':False,'room':'https://fixture.invalid','refused':'fixture pairing revoked'})
                revoked = await wait_refusal_status(facade, http_thread, 115, True)
                assert revoked['joined'] is False and revoked.get('closed_by_room') is True
                assert 'pairing was revoked' in revoked.get('note', ''), revoked
                revoked_say = await tool_failure(facade, 'voice_say', {'conversation':http_thread,
                    'session_id':'typed:' + http_thread, 'revision':0, 'text':'Must not be published'}, 116)
                assert 'pairing was revoked' in revoked_say, revoked_say
                revoked_join = await tool_failure(facade, 'voice_connect', {'title':'Blocked while revoked'}, 117)
                assert 'revoked this connector pairing' in revoked_join, revoked_join
                core_json(core_data / 'local.sock', 'POST', '/api/test/rendezvous-state',
                    {'connected':False,'room':None,'refused':None})
                await wait_refusal_status(facade, http_thread, 123, False)
                reopened = await tool(facade, 'voice_connect', {'title':'HTTP after re-pair'}, 118)
                assert reopened['conversation'] == http_thread, reopened
                await wait_core_connected(facade, http_thread, 122)
                await tool(facade, 'voice_disconnect', {}, 119)
                await finish(facade)
                facades.remove(facade)

                # Cursor editor view route: verify MCP App negotiation, resource metadata, delivery and read.
                UI_CAPS = {'extensions': {'io.modelcontextprotocol/ui': {'mimeTypes':['text/html']}}}
                editor_caps = {'deliver':'supported','inspectInbound':'unsupported','working':'supported','endOfTurn':'supported','sessionIdentity':'supported'}
                app_facade, listed = await start_facade(binary, root, base_env, 'cursor-vscode', UI_CAPS)
                facades.append(app_facade)
                tool_info = next(item for item in listed['tools'] if item['name'] == 'voice_connect')
                assert tool_info.get('_meta', {}).get('ui', {}).get('resourceUri') == 'ui://sidevoice/voice-link', tool_info
                resources = await mcp_request(app_facade, 'resources/list', {}, 5)
                assert resources['resources'][0]['uri'] == 'ui://sidevoice/voice-link', resources
                resource = await mcp_request(app_facade, 'resources/read', {'uri':'ui://sidevoice/voice-link'}, 6)
                assert resource['contents'][0]['mimeType'] == 'text/html;profile=mcp-app', resource
                assert resource['contents'][0]['_meta']['ui']['prefersBorder'] is True, resource
                app_joined = await tool(app_facade, 'voice_connect', {'title':'Cursor card'}, 34)
                assert app_joined.get('card', {}).get('requested') is True, app_joined
                assert app_joined['capabilities'] == editor_caps and 'deliver' in app_joined['experimental'], app_joined
                app_thread = app_joined['conversation']
                app_status = await tool(app_facade, 'voice_status', {'conversation':app_thread}, 35)
                assert app_status['joined'] is True and app_status['capabilities'] == editor_caps, app_status
                app_query = urlencode({'thread':app_thread, 'auth':hmac.new(bytes.fromhex(app_joined['view_link']['key']),
                    ('poll:' + app_thread).encode(), hashlib.sha256).hexdigest()})
                # Partial unauthenticated requests cannot occupy every card slot indefinitely.
                stalled = []
                try:
                    for _ in range(32):
                        connection = socket.create_connection(('127.0.0.1', app_joined['view_link']['port']), timeout=2)
                        connection.sendall(b'GET /cursor-app/next HTTP/1.1\r\n')
                        stalled.append(connection)
                    await asyncio.sleep(.3)
                    overflow = socket.create_connection(('127.0.0.1', app_joined['view_link']['port']), timeout=2)
                    try:
                        overflow.sendall(b'GET /cursor-app/next HTTP/1.1\r\n')
                        try:
                            rejected = await asyncio.to_thread(overflow.recv, 1)
                        except ConnectionResetError:
                            rejected = b''
                        assert rejected == b'', 'card accepted an unbounded client'
                    finally:
                        overflow.close()
                finally:
                    held = stalled.pop() if stalled else None
                    for connection in stalled:
                        connection.close()
                if held:
                    try:
                        held.settimeout(7)
                        try:
                            timed_out = await asyncio.to_thread(held.recv, 1)
                        except ConnectionResetError:
                            timed_out = b''
                        assert timed_out == b'', 'partial card request did not expire'
                    finally:
                        held.close()
                async def card_capacity_recovered():
                    try:
                        await asyncio.to_thread(card_probe, app_joined['view_link']['port'], app_query)
                        return True
                    except (OSError, AssertionError):
                        return False
                await until_async(card_capacity_recovered, 'Cursor card capacity after partial requests', seconds=6)
                app_poll = asyncio.create_task(asyncio.to_thread(card_get, app_joined['view_link']['port'], app_query))
                await asyncio.sleep(.1)
                app_row = await core_text(core_data / 'local.sock', token, session, app_thread,
                    'Core input for Cursor editor card')
                card_message = await asyncio.wait_for(app_poll, 5)
                assert card_message['message_id'] == app_row['message_id'] and 'Core input for Cursor editor card' in card_message['text'], card_message
                expected_sig = hmac.new(bytes.fromhex(app_joined['view_link']['key']),
                    ('msg:' + card_message['message_id'] + '\n' + card_message['text']).encode(), hashlib.sha256).hexdigest()
                assert card_message['sig'] == expected_sig, card_message
                await asyncio.to_thread(done_card, app_joined['view_link']['port'], app_query,
                    {'message_id':app_row['message_id'],'stage':'dispatched','ok':True})
                await wait_status(core_data / 'local.sock', token, app_thread, app_row['id'], 'unconfirmed')
                await status_and_reply(app_facade, app_thread, app_joined, app_row, editor_caps, 36)
                await asyncio.to_thread(done_card, app_joined['view_link']['port'], app_query,
                    {'message_id':app_row['message_id'],'stage':'answered','ok':True})
                await wait_status(core_data / 'local.sock', token, app_thread, app_row['id'], 'read')
                # A new MCP process adopts the detached editor binding by its private conversation id.
                await finish(app_facade)
                facades.remove(app_facade)
                app_facade, _ = await start_facade(binary, root, base_env, 'cursor-vscode', UI_CAPS)
                facades.append(app_facade)
                adopted = await tool(app_facade, 'voice_status', {'conversation':app_thread}, 37)
                assert adopted['joined'] is True and adopted['binding_id'] == app_joined['binding_id'], adopted
                old_daemon = daemon
                await finish(old_daemon)
                daemon = None
                (data / 'connector.sock').unlink(missing_ok=True)
                daemon = await asyncio.create_subprocess_exec(str(binary), '--profile-root', str(root), 'connector',
                    stdout=asyncio.subprocess.DEVNULL, stderr=core_log, env=base_env)
                await until((data / 'connector.sock').exists, 'restarted Rust connector after adopted binding')
                replayed = await until_async(lambda: adopted_status(app_facade, app_thread), 'adopted binding replay')
                assert replayed['joined'] is True and replayed['binding_id'] != adopted['binding_id'], replayed
                replay_link = replayed.get('view_link')
                assert replay_link and replay_link.get('port') and replay_link.get('key') == app_joined['view_link']['key'], replayed
                assert replay_link['port'] == app_joined['view_link']['port'], (app_joined['view_link'], replay_link)
                replay_query = urlencode({'thread':app_thread, 'auth':hmac.new(bytes.fromhex(replay_link['key']),
                    ('poll:' + app_thread).encode(), hashlib.sha256).hexdigest()})
                app_row = await core_text(core_data / 'local.sock', token, session, app_thread,
                    'Core input after Cursor connector restart')
                app_poll = asyncio.create_task(asyncio.to_thread(card_get, app_joined['view_link']['port'], app_query))
                card_message = await asyncio.wait_for(app_poll, 5)
                assert card_message['message_id'] == app_row['message_id'] and 'Core input after Cursor connector restart' in card_message['text'], card_message
                await asyncio.to_thread(done_card, app_joined['view_link']['port'], app_query,
                    {'message_id':app_row['message_id'],'stage':'dispatched','ok':True})
                await wait_status(core_data / 'local.sock', token, app_thread, app_row['id'], 'unconfirmed')
                await status_and_reply(app_facade, app_thread, {**app_joined,'binding_id':replayed['binding_id']},
                    app_row, editor_caps, 149)
                await asyncio.to_thread(done_card, app_joined['view_link']['port'], app_query,
                    {'message_id':app_row['message_id'],'stage':'answered','ok':True})
                await wait_status(core_data / 'local.sock', token, app_thread, app_row['id'], 'read')
                await tool(app_facade, 'voice_disconnect', {'conversation':app_thread}, 38)
                app_again = await tool(app_facade, 'voice_connect', {'title':'Cursor card reconnect'}, 39)
                await tool(app_facade, 'voice_disconnect', {'conversation':app_again['conversation']}, 40)
                await finish(app_facade)
                facades.remove(app_facade)

                # Cursor Desktop Bridge is preferred only after its per-conversation key identifies one chat.
                received_bridge = asyncio.Queue()
                bridge_chat = str(uuid.uuid4())
                stale_chat = str(uuid.uuid4())
                received_bridge.listed_chat = stale_chat
                bridge_transcript = home / '.cursor' / 'projects' / 'synthetic-workspace' / 'agent-transcripts' / bridge_chat / f'{bridge_chat}.jsonl'
                stale_transcript = home / '.cursor' / 'projects' / 'old-workspace' / 'agent-transcripts' / stale_chat / f'{stale_chat}.jsonl'
                make_cursor_store(cursor_config, workspace, bridge_chat, 'cursor-bridge-fixture-model')
                bridge_server, _bridge_file = await start_bridge(bridge_dir, bridge_user_data, bridge_socket,
                    bridge_chat, received_bridge, bridge_transcript)
                bridge_facade, listed = await start_facade(binary, root, base_env, 'cursor-vscode', UI_CAPS)
                facades.append(bridge_facade)
                bridge_joined = await tool(bridge_facade, 'voice_connect', {'title':'Cursor Desktop Bridge'}, 45)
                assert bridge_joined['card']['bridge'] is True and bridge_joined['view_link'], bridge_joined
                bridge_thread = bridge_joined['conversation']
                bridge_key = bridge_joined['view_link']['key']
                append_cursor_connect(stale_transcript, 'Cursor Desktop Bridge')
                await asyncio.sleep(.25)  # let every scheduled bridge-state lookup miss
                late_status = await tool(bridge_facade, 'voice_status', {'conversation':bridge_thread}, 46)
                assert late_status['card']['bridge_chat_known'] is False, late_status
                state = sqlite3.connect(state_db)
                state.execute('INSERT OR REPLACE INTO cursorDiskKV(key,value) VALUES(?,?)',
                    (f'bubbleId:{bridge_chat}:result', json.dumps({'voice_connect':bridge_key})))
                state.commit()
                state.close()
                await wait_bridge_chat(bridge_facade, bridge_thread, 148)
                bridge_row = await core_text(core_data / 'local.sock', token, session, bridge_thread,
                    'Core input for Cursor Desktop Bridge')
                sent = await asyncio.wait_for(received_bridge.get(), 12)
                assert sent['threadId'] == bridge_chat and 'Core input for Cursor Desktop Bridge' in sent['text'], sent
                await wait_status(core_data / 'local.sock', token, bridge_thread, bridge_row['id'], 'delivered')
                append_cursor(bridge_transcript, sent['text'])
                await wait_status(core_data / 'local.sock', token, bridge_thread, bridge_row['id'], 'read', 15)
                await wait_engine(core_data / 'local.sock', session, bridge_thread, 'cursor-bridge-fixture-model')
                await status_and_reply(bridge_facade, bridge_thread, bridge_joined, bridge_row, editor_caps, 47)
                await tool(bridge_facade, 'voice_disconnect', {'conversation':bridge_thread}, 48)
                bridge_reconnect = await tool(bridge_facade, 'voice_connect', {'title':'Cursor Bridge reconnect'}, 49)
                assert bridge_reconnect['conversation'] != bridge_thread and bridge_reconnect['capabilities'] == editor_caps, bridge_reconnect
                await tool(bridge_facade, 'voice_disconnect', {'conversation':bridge_reconnect['conversation']}, 50)
                await finish(bridge_facade)
                facades.remove(bridge_facade)

                # A Claude session that bypasses prompts without accepting cross-process input is held/refused.
                held_id = str(uuid.uuid4())
                fake_cli = root / 'fake-claude-session.py'
                fake_cli.write_text('import time\ntime.sleep(120)\n')
                held_process = await asyncio.create_subprocess_exec(sys.executable, str(fake_cli), '--permission-mode',
                    'bypassPermissions', stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL)
                (sessions / 'held.json').write_text(json.dumps({'sessionId':held_id,'pid':held_process.pid,'status':'idle'}))
                held_env = {**base_env, 'CLAUDE_CODE_SESSION_ID':held_id,
                    'CLAUDE_CODE_MESSAGING_SOCKET':str(claude_socket), 'CLAUDE_CODE_MESSAGING_TOKEN':claude_token}
                held_facade, _ = await start_facade(binary, root, held_env, 'claude')
                facades.append(held_facade)
                refused = await tool_failure(held_facade, 'voice_connect', {'title':'Held Claude'}, 50)
                assert 'bypasses permission prompts' in refused and 'safeguard it removes' in refused.lower(), refused
                await finish(held_facade)
                facades.remove(held_facade)
                held_process.terminate()
                await held_process.wait()
                held_process = None

                # Oversized legacy closure state blocks automatic replay but permits an explicit rejoin.
                history_thread = str(uuid.uuid4())
                history_env = {**base_env, 'SIDEVOICE_THREAD':history_thread,
                    'SIDEVOICE_DELIVERY_URL':f'http://127.0.0.1:{receiver.server_port}/history-limit'}
                history_facade, _ = await start_facade(binary, root, history_env, 'fixture-http')
                facades.append(history_facade)
                history_joined = await tool(history_facade, 'voice_connect', {'title':'History bound fixture'}, 150)
                await finish(daemon)
                daemon = None
                (data / 'connector.sock').unlink(missing_ok=True)
                oversized_state = data / 'conversation-state.json'
                oversized_state.write_text(json.dumps({'closed_by_room':{
                    'legacy-' + ('x' * (1 << 20)):'closed_from_room'}}))
                oversized_state.chmod(0o600)
                daemon = await asyncio.create_subprocess_exec(str(binary), '--profile-root', str(root), 'connector',
                    stdout=asyncio.subprocess.DEVNULL, stderr=core_log, env=base_env)
                await until((data / 'connector.sock').exists, 'Rust connector restarted with oversized legacy state')
                await until(lambda: json.loads(oversized_state.read_text()).get('resume_blocked') is True,
                    'oversized state compacted and resume blocked')
                old_status = await tool(history_facade, 'voice_status', {'conversation':history_thread}, 151)
                assert old_status['joined'] is False and 'history limit' in old_status['note'], old_status
                old_speech = await tool_failure(history_facade, 'voice_say', {
                    'conversation':history_thread,'session_id':'history:legacy','revision':0,'text':'stale reply'}, 155)
                assert 'history limit' in old_speech, old_speech
                other_thread = str(uuid.uuid4())
                other_env = {**base_env, 'SIDEVOICE_THREAD':other_thread,
                    'SIDEVOICE_DELIVERY_URL':f'http://127.0.0.1:{receiver.server_port}/history-peer'}
                other_facade, _ = await start_facade(binary, root, other_env, 'fixture-http')
                facades.append(other_facade)
                await tool(other_facade, 'voice_connect', {'title':'Other façade'}, 160)
                unnamed_status = await tool(history_facade, 'voice_status', {}, 156)
                assert unnamed_status['joined'] is False and 'history limit' in unnamed_status.get('note', ''), unnamed_status
                unnamed_speech = await tool_failure(history_facade, 'voice_say', {
                    'session_id':'history:legacy','revision':0,'text':'stale unnamed reply'}, 157)
                assert 'history limit' in unnamed_speech, unnamed_speech
                history_rejoined = await tool(history_facade, 'voice_connect', {'title':'Explicit history rejoin'}, 152)
                assert history_rejoined['conversation'] == history_thread, history_rejoined
                rejoined_status = await tool(history_facade, 'voice_status', {'conversation':history_thread}, 153)
                assert rejoined_status['joined'] is True and rejoined_status['connector']['resume_blocked'] is True, rejoined_status
                await tool(other_facade, 'voice_disconnect', {'conversation':other_thread}, 161)
                await finish(other_facade)
                facades.remove(other_facade)

                # If the closure-state rename fails, the preallocated guard still blocks replay after restart.
                state_path = data / 'conversation-state.json'
                state_backup = data / 'conversation-state.backup'
                state_path.replace(state_backup)
                state_backup.write_text(json.dumps({'refused':None,'closed_by_room':{},
                    'resume_blocked':False,'resume_block_reason':None}))
                state_backup.chmod(0o600)
                state_path.mkdir(mode=0o700)
                core_json(core_data / 'local.sock', 'POST', '/api/presentation/select',
                    {'session_id':session,'thread_id':history_thread}, token)
                core_json(core_data / 'local.sock', 'POST', '/api/presentation/close',
                    {'thread_id':history_thread}, token)
                guard_path = data / 'conversation-state.guard'
                await until(lambda: guard_path.read_bytes()[8] == 1, 'emergency replay guard after state write failure')
                failed_write_status = await wait_closed_status(history_facade, history_thread, 162)
                assert 'closed this conversation' in failed_write_status.get('note', ''), failed_write_status
                state_path.rmdir()
                state_backup.replace(state_path)
                await finish(daemon)
                daemon = None
                (data / 'connector.sock').unlink(missing_ok=True)
                daemon = await asyncio.create_subprocess_exec(str(binary), '--profile-root', str(root), 'connector',
                    stdout=asyncio.subprocess.DEVNULL, stderr=core_log, env=base_env)
                await until((data / 'connector.sock').exists, 'Rust connector restarted under emergency replay guard')
                failed_write_replay = await tool(history_facade, 'voice_status', {'conversation':history_thread}, 163)
                assert failed_write_replay['joined'] is False and 'could not save conversation state' in failed_write_replay.get('note', ''), failed_write_replay
                failed_write_say = await tool_failure(history_facade, 'voice_say', {
                    'conversation':history_thread,'session_id':'history:legacy','revision':0,'text':'unsafe replay'}, 164)
                assert 'could not save conversation state' in failed_write_say, failed_write_say
                history_rejoined = await tool(history_facade, 'voice_connect', {'title':'Explicit storage recovery'}, 165)
                assert history_rejoined['conversation'] == history_thread, history_rejoined
                await wait_core_connected(history_facade, history_thread, 166)
                core_json(core_data / 'local.sock', 'POST', '/api/test/rendezvous-state',
                    {'connected':False,'room':'https://fixture.invalid','refused':'fixture pairing revoked'})
                revoked_after_latch = await wait_refusal_status(history_facade, history_thread, 167, True)
                assert 'pairing was revoked' in revoked_after_latch.get('note', ''), revoked_after_latch
                revoked_after_latch_say = await tool_failure(history_facade, 'voice_say', {'conversation':history_thread,
                    'session_id':'history:legacy','revision':0,'text':'must stay refused'}, 168)
                assert 'pairing was revoked' in revoked_after_latch_say, revoked_after_latch_say
                core_json(core_data / 'local.sock', 'POST', '/api/test/rendezvous-state',
                    {'connected':False,'room':None,'refused':None})
                await wait_refusal_status(history_facade, history_thread, 169, False)
                history_rejoined = await tool(history_facade, 'voice_connect', {'title':'History after re-pair'}, 170)
                assert history_rejoined['conversation'] == history_thread, history_rejoined
                await wait_core_connected(history_facade, history_thread, 171)
                core_json(core_data / 'local.sock', 'POST', '/api/presentation/select',
                    {'session_id':session,'thread_id':history_thread}, token)
                core_json(core_data / 'local.sock', 'POST', '/api/presentation/close',
                    {'thread_id':history_thread}, token)
                post_latch_close = await wait_closed_status(history_facade, history_thread, 172)
                assert 'closed this conversation' in post_latch_close.get('note', ''), post_latch_close
                post_latch_say = await tool_failure(history_facade, 'voice_say', {'conversation':history_thread,
                    'session_id':'history:legacy','revision':0,'text':'must remain closed'}, 173)
                assert 'closed this conversation' in post_latch_say, post_latch_say
                await finish(history_facade)
                facades.remove(history_facade)

                print(json.dumps({'pinned_core':launch_id,'mcp_tools':6,
                    'routes':{'claude':'unknown_then_read','codex':'accepted_then_read',
                        'codex_http':'accepted_then_read',
                        'cursor_cli_persist':'unknown_then_read','cursor_cli_plain':'unsupported',
                        'cursor_editor_card':'unknown_then_read','cursor_desktop_bridge':'accepted_then_read',
                        'generic_http':'accepted_without_read'},
                    'binding_disconnect_reconnect':True,'detached_editor_adopt':True,
                    'foreign_owner_refused':True,'claude_inbound_hold_refused':True,
                    'room_close_and_revocation':True,'working_replay_state':True,'engine_observation':True,
                    'conversation_history_limit_fail_closed':True,'state_write_failure_guard':True,
                    'post_latch_room_close_and_revocation':True,'cursor_card_survives_restart':True,
                    'paid_prompts':False}))
        except Exception:
            core_log.flush()
            print('Core returncode:', core.returncode, file=sys.stderr)
            print('Rust daemon returncode:', daemon.returncode if daemon else None, file=sys.stderr)
            for name in ('core.log','core-app.log'):
                path = root / name
                if path.exists():
                    print(name + ':\n' + path.read_text(errors='replace')[-5000:], file=sys.stderr)
            raise
        finally:
            if held_process and held_process.returncode is None:
                held_process.terminate()
                await held_process.wait()
            for facade in list(facades):
                await finish(facade)
            if bridge_server:
                bridge_server.close()
                await bridge_server.wait_closed()
            if claude_server:
                claude_server.close()
                await claude_server.wait_closed()
            await finish(daemon)
            await finish(core)
            receiver.shutdown()
            receiver.server_close()
            receiver_thread.join(timeout=3)
            store_handle.close()
            core_log.close()


def card_get(port, query):
    connection = http.client.HTTPConnection('127.0.0.1', port, timeout=30)
    connection.request('GET', '/cursor-app/next?' + query, headers={'Origin':'vscode-webview://cursor'})
    response = connection.getresponse()
    payload = response.read()
    connection.close()
    if response.status != 200:
        raise AssertionError(f'Cursor card poll returned HTTP {response.status}')
    return json.loads(payload)


def card_probe(port, query):
    connection = http.client.HTTPConnection('127.0.0.1', port, timeout=5)
    connection.request('GET', '/cursor-app/unknown?' + query, headers={'Origin':'vscode-webview://cursor'})
    response = connection.getresponse()
    response.read()
    connection.close()
    assert response.status == 404, response.status


def done_card(port, query, outcome):
    connection = http.client.HTTPConnection('127.0.0.1', port, timeout=5)
    connection.request('POST', '/cursor-app/done?' + query, json.dumps(outcome).encode(),
        headers={'Origin':'vscode-webview://cursor','Content-Type':'text/plain'})
    response = connection.getresponse()
    response.read()
    connection.close()
    assert response.status == 204, response.status


CLAUDE_CAPS = {'deliver':'supported','inspectInbound':'supported','working':'supported','endOfTurn':'supported','sessionIdentity':'supported'}
CODEX_CAPS = {'deliver':'supported','inspectInbound':'unsupported','working':'supported','endOfTurn':'supported','sessionIdentity':'supported'}
CURSOR_CAPS = {'deliver':'supported','inspectInbound':'unsupported','working':'supported','endOfTurn':'supported','sessionIdentity':'supported'}


if __name__ == '__main__':
    asyncio.run(parity_fixture())
