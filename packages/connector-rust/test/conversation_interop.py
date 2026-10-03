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


async def core_text(core_socket, token, session, binding, thread, phrase):
    message_id = str(uuid.uuid4())
    result = core_json(core_socket, 'POST', '/api/presentation/text', {
        'text': phrase, 'session_id': session, 'thread_id': thread,
        'binding_id': binding, 'message_id': message_id}, token)
    assert result.get('accepted') is True, result
    return result


def history(core_socket, token, thread):
    return core_json(core_socket, 'GET', '/api/presentation/history?thread_id=' + thread, token=token)['messages']


async def wait_status(core_socket, token, thread, message_id, status, seconds=30):
    return await until(lambda: next((row for row in history(core_socket, token, thread)
        if row.get('id') == message_id and row.get('status') == status), None),
        f'{thread} {message_id} status {status}', seconds)


async def status_and_reply(process, thread, joined, input_row, expected_caps, request_id):
    status = await tool(process, 'voice_status', {'conversation': thread}, request_id)
    assert status['joined'] is True and status['capabilities'] == expected_caps, status
    assert status['binding_id'] == joined['binding_id'], status
    assert isinstance(status['room_reachable'], bool) and status['room_reachable'] == status['connector']['connected'], status
    said = await tool(process, 'voice_say', {'session_id': input_row['session_id'],
        'revision': input_row['revision'], 'text': 'A synthetic reply for ' + thread}, request_id + 1)
    assert said.get('text_saved') is True and said.get('status') in {'published', 'queued'}, said
    return said


def append_claude(transcript, text):
    transcript.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    with transcript.open('a') as output:
        output.write(json.dumps({'type': 'user', 'message': {'role': 'user', 'content': text}}) + '\n')


def append_cursor(transcript, text):
    transcript.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    with transcript.open('a') as output:
        output.write(json.dumps({'role': 'user', 'message': {'content': [{'type': 'text', 'text': text}]}}) + '\n')


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
                response = {'threads': [{'id': chat, 'title': 'Synthetic Cursor', 'source': 'local', 'status': 'running'}]}
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
            joined['binding_id'], thread, 'Core input for ' + route)
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
        home, claude_home, codex_home, cursor_config = (root / name for name in ('home', 'claude', 'codex', 'cursor'))
        data = root / 'sidevoice'
        core_data = data / 'core'
        tool_dir = root / 'bin'
        for directory in (home, claude_home, codex_home, cursor_config, data, core_data, tool_dir):
            directory.mkdir(parents=True, mode=0o700, exist_ok=True)
            directory.chmod(0o700)
        cursor_chat = str(uuid.uuid4())
        test_sqlite_cli(root)
        codex_bin, tmux_bin = fake_tools(root, cursor_chat)
        workspace = 'a' * 32
        store = cursor_config / 'chats' / workspace / cursor_chat / 'store.db'
        store.parent.mkdir(parents=True, mode=0o700)
        store.touch(mode=0o600)
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
            'CODEX_HOME': str(codex_home), 'CURSOR_CONFIG_DIR': str(cursor_config), 'CURSOR_DATA_DIR': str(home / '.cursor'),
            'CURSOR_DESKTOP_BRIDGE_DIR': str(bridge_dir), 'SIDEVOICE_DATA_DIR': str(data),
            'SIDEVOICE_SERVICE_MANAGER': 'none', 'SIDEVOICE_CODEX_BIN': str(codex_bin),
            'SIDEVOICE_CURSOR_LOOKUP_AT': '25,50,75,100', 'SIDEVOICE_CURSOR_SCAN_MS': '10',
            'SIDEVOICE_TMUX_BIN': str(tmux_bin), 'SIDEVOICE_TEST_CODEX_QUEUE': str(codex_queue),
            'PATH': str(tool_dir) + os.pathsep + os.environ.get('PATH', '/usr/bin:/bin')}
        core_log = (root / 'core.log').open('wb')
        launch_id = str(uuid.uuid4())
        core = await asyncio.create_subprocess_exec(str(python), '-m', 'sidevoice_core.server', '--data-dir', str(core_data),
            '--socket', str(core_data / 'local.sock'), '--port', '0', '--idle-exit', '0', '--launch-id', launch_id,
            '--log-file', str(root / 'core-app.log'), stdout=core_log, stderr=core_log, env=base_env)
        daemon = None
        bridge_server = None
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
                presentation = {'socket': core_data / 'local.sock', 'token': token, 'session': session}

                async def claude_after(facade, joined, row, route_env):
                    envelope = await asyncio.wait_for(received_claude.get(), 5)
                    assert 'Core input for Claude' in envelope and '"message_id"' in envelope, envelope
                    await wait_status(core_data / 'local.sock', token, joined['conversation'], row['id'], 'unconfirmed')
                    await wait_status(core_data / 'local.sock', token, joined['conversation'], row['id'], 'read')
                    await status_and_reply(facade, joined['conversation'], joined, row, CLAUDE_CAPS, 40)

                async def codex_after(facade, joined, row, route_env):
                    queued = await until(lambda: json.loads(codex_queue.read_text()) if codex_queue.exists() else None, 'Codex fixture queue')
                    assert queued['thread'] == codex_thread and 'Core input for Codex' in queued['message'], queued
                    await wait_status(core_data / 'local.sock', token, joined['conversation'], row['id'], 'delivered')
                    rollout.write_text(json.dumps({'type': 'event_msg', 'payload': {'type': 'task_started', 'turn_id': 'fixture-turn'}}) + '\n'
                        + json.dumps({'type': 'response_item', 'payload': {'type': 'message', 'role': 'user',
                            'content': [{'type': 'input_text', 'text': queued['message']}]}}) + '\n'
                        + json.dumps({'type': 'event_msg', 'payload': {'type': 'task_complete', 'turn_id': 'fixture-turn'}}) + '\n')
                    await wait_status(core_data / 'local.sock', token, joined['conversation'], row['id'], 'read')
                    await status_and_reply(facade, joined['conversation'], joined, row, CODEX_CAPS, 50)

                async def cursor_cli_after(facade, joined, row, route_env):
                    pasted = await until(lambda: cursor_sent.read_text() if cursor_sent.exists() else None, 'Cursor persist paste')
                    assert 'Core input for Cursor persist' in pasted, pasted
                    await wait_status(core_data / 'local.sock', token, joined['conversation'], row['id'], 'unconfirmed')
                    append_cursor(cursor_transcript, pasted)
                    await wait_status(core_data / 'local.sock', token, joined['conversation'], row['id'], 'read')
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
                codex_env = {'CODEX_THREAD_ID': codex_thread, 'SIDEVOICE_TEST_CODEX_QUEUE': str(codex_queue)}
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
                    'SIDEVOICE_DELIVERY_URL':f'http://127.0.0.1:{receiver.server_port}/codex-input'}
                async def codex_http_after(http_facade, http_joined, http_row, _route_env):
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
                input_row = await core_text(core_data / 'local.sock', token, session, joined['binding_id'], cursor_chat, 'Core input for Cursor persist')
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
                plain_row = await core_text(core_data / 'local.sock', token, session, held['binding_id'], cursor_chat, 'Held unsupported Cursor message')
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
                restarted_row = await core_text(core_data / 'local.sock', token, session, rebound['binding_id'],
                    http_thread, 'Core input after connector restart')
                restarted_request = await asyncio.to_thread(receiver.messages.get, True, 10)
                assert restarted_request['thread_id'] == http_thread and restarted_request['text'] == 'Core input after connector restart', restarted_request
                await wait_status(core_data / 'local.sock', token, http_thread, restarted_row['id'], 'delivered')
                await tool(facade, 'voice_disconnect', {}, 109)
                reconnect = await tool(facade, 'voice_connect', {'title':'HTTP reconnect'}, 110)
                assert reconnect['conversation'] == http_thread and reconnect['capabilities'] == HTTP_CAPS, reconnect
                await tool(facade, 'voice_disconnect', {}, 111)
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
                app_poll = asyncio.create_task(asyncio.to_thread(card_get, app_joined['view_link']['port'], app_query))
                await asyncio.sleep(.1)
                app_row = await core_text(core_data / 'local.sock', token, session, app_joined['binding_id'], app_thread,
                    'Core input for Cursor editor card')
                card_message = await asyncio.wait_for(app_poll, 5)
                assert card_message['message_id'] == app_row['message_id'] and 'Core input for Cursor editor card' in card_message['text'], card_message
                expected_sig = hmac.new(bytes.fromhex(app_joined['view_link']['key']),
                    ('msg:' + card_message['message_id'] + '\n' + card_message['text']).encode(), hashlib.sha256).hexdigest()
                assert card_message['sig'] == expected_sig, card_message
                await done_card(app_joined['view_link']['port'], app_query, {'message_id':app_row['message_id'],'stage':'dispatched','ok':True})
                await wait_status(core_data / 'local.sock', token, app_thread, app_row['id'], 'unconfirmed')
                await status_and_reply(app_facade, app_thread, app_joined, app_row, editor_caps, 36)
                await done_card(app_joined['view_link']['port'], app_query, {'message_id':app_row['message_id'],'stage':'answered','ok':True})
                await wait_status(core_data / 'local.sock', token, app_thread, app_row['id'], 'read')
                # A new MCP process adopts the detached editor binding by its private conversation id.
                await finish(app_facade)
                facades.remove(app_facade)
                app_facade, _ = await start_facade(binary, root, base_env, 'cursor-vscode', UI_CAPS)
                facades.append(app_facade)
                adopted = await tool(app_facade, 'voice_status', {'conversation':app_thread}, 37)
                assert adopted['joined'] is True and adopted['binding_id'] == app_joined['binding_id'], adopted
                await tool(app_facade, 'voice_disconnect', {'conversation':app_thread}, 38)
                app_again = await tool(app_facade, 'voice_connect', {'title':'Cursor card reconnect'}, 39)
                await tool(app_facade, 'voice_disconnect', {'conversation':app_again['conversation']}, 40)
                await finish(app_facade)
                facades.remove(app_facade)

                # Cursor Desktop Bridge is preferred only after its per-conversation key identifies one chat.
                received_bridge = asyncio.Queue()
                bridge_chat = str(uuid.uuid4())
                bridge_server, _bridge_file = await start_bridge(bridge_dir, bridge_user_data, bridge_socket,
                    bridge_chat, received_bridge, cursor_transcript)
                bridge_facade, listed = await start_facade(binary, root, base_env, 'cursor-vscode', UI_CAPS)
                facades.append(bridge_facade)
                bridge_joined = await tool(bridge_facade, 'voice_connect', {'title':'Cursor Desktop Bridge'}, 45)
                assert bridge_joined['card']['bridge'] is True and bridge_joined['view_link'], bridge_joined
                bridge_thread = bridge_joined['conversation']
                bridge_key = bridge_joined['view_link']['key']
                await asyncio.sleep(.25)  # let every scheduled bridge-state lookup miss
                late_status = await tool(bridge_facade, 'voice_status', {'conversation':bridge_thread}, 46)
                assert late_status['card']['bridge_chat_known'] is False, late_status
                state = sqlite3.connect(state_db)
                state.execute('INSERT OR REPLACE INTO cursorDiskKV(key,value) VALUES(?,?)',
                    (f'bubbleId:{bridge_chat}:result', json.dumps({'voice_connect':bridge_key})))
                state.commit()
                state.close()
                bridge_row = await core_text(core_data / 'local.sock', token, session, bridge_joined['binding_id'], bridge_thread,
                    'Core input for Cursor Desktop Bridge')
                sent = await asyncio.wait_for(received_bridge.get(), 12)
                assert sent['threadId'] == bridge_chat and 'Core input for Cursor Desktop Bridge' in sent['text'], sent
                await wait_status(core_data / 'local.sock', token, bridge_thread, bridge_row['id'], 'delivered')
                append_cursor(cursor_transcript, sent['text'])
                await wait_status(core_data / 'local.sock', token, bridge_thread, bridge_row['id'], 'read', 15)
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

                print(json.dumps({'pinned_core':launch_id,'mcp_tools':6,
                    'routes':{'claude':'unknown_then_read','codex':'accepted_then_read',
                        'codex_http':'accepted_then_read',
                        'cursor_cli_persist':'unknown_then_read','cursor_cli_plain':'unsupported',
                        'cursor_editor_card':'unknown_then_read','cursor_desktop_bridge':'accepted_then_read',
                        'generic_http':'accepted_without_read'},
                    'binding_disconnect_reconnect':True,'detached_editor_adopt':True,
                    'foreign_owner_refused':True,'claude_inbound_hold_refused':True,
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
