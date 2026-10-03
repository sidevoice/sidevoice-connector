"""Hosted real-Core/v3 test with a deliberately modelled Codex queue.

This checks Core-originated input, Core acknowledgements, the Rust façade and
outbox. It does not claim a real authenticated Codex session; that is a
separate acceptance gate.
"""

import asyncio
import http.client
import json
import os
from pathlib import Path
import shutil
import socket
import sys
import tempfile
import time
import uuid

from websockets.asyncio.client import unix_connect


class UnixHTTP(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__('localhost', timeout=3)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def http_json(socket_path, method, path, body=None, token=None):
    connection = UnixHTTP(str(socket_path))
    headers = {'Host': 'localhost'}
    if body is not None:
        headers['Content-Type'] = 'application/json'
    if token:
        headers['Authorization'] = 'Bearer ' + token
    connection.request(method, path, json.dumps(body).encode() if body is not None else None, headers)
    response = connection.getresponse()
    payload = response.read()
    connection.close()
    result = json.loads(payload) if payload else None
    if response.status >= 400:
        raise AssertionError(f'{method} {path}: HTTP {response.status} {result}')
    return result


async def until(predicate, description, seconds=30):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        await asyncio.sleep(.1)
    raise AssertionError(f'timed out: {description}')


async def mcp_request(process, method, params, id):
    process.stdin.write((json.dumps({'jsonrpc': '2.0', 'id': id, 'method': method, 'params': params}) + '\n').encode())
    await process.stdin.drain()
    while True:
        line = await asyncio.wait_for(process.stdout.readline(), 20)
        if not line:
            raise AssertionError(f'MCP closed waiting for {method}')
        result = json.loads(line)
        if result.get('id') == id:
            if 'error' in result:
                raise AssertionError(f'MCP {method}: {result["error"]}')
            return result['result']


async def tool(process, name, args, id, thread=None):
    params = {'name': name, 'arguments': args}
    if thread:
        params['_meta'] = {'openai/threadId': thread}
    result = await mcp_request(process, 'tools/call', params, id)
    if result.get('isError'):
        raise AssertionError(f'{name}: {result}')
    return json.loads(result['content'][0]['text'])


async def finish(process):
    if process.returncode is None:
        process.terminate()
        try:
            await asyncio.wait_for(process.wait(), 8)
        except asyncio.TimeoutError:
            process.kill()
            await process.wait()


def core_faults(root, env):
    """Faults live only in this temporary Core process, with no production hook."""
    storage, late, slow = root / 'fail-journal-once', root / 'delay-ack-once', root / 'slow-two-acks'
    (root / 'sitecustomize.py').write_text('''import asyncio, os
from pathlib import Path
from sidevoice_core.control.history import RoomHistory
from sidevoice_core.control.room import Room
_put = RoomHistory.put
_publish = Room.publish
def put(self, **fields):
    flag = Path(os.environ['SIDEVOICE_TEST_STORAGE_FLAG'])
    if fields.get('role') == 'assistant' and flag.exists():
        flag.unlink()
        raise RuntimeError('isolated injected journal failure')
    return _put(self, **fields)
async def publish(self, payload):
    result = await _publish(self, payload)
    flag = Path(os.environ['SIDEVOICE_TEST_LATE_ACK_FLAG'])
    if flag.exists():
        flag.unlink()
        await asyncio.sleep(17)
    slow = Path(os.environ['SIDEVOICE_TEST_SLOW_ACK_FLAG'])
    if slow.exists():
        remaining = int(slow.read_text())
        if remaining > 1:
            slow.write_text(str(remaining - 1))
        else:
            slow.unlink()
        await asyncio.sleep(12)
    return result
RoomHistory.put = put
Room.publish = publish
''')
    return {**env, 'PYTHONPATH': str(root) + os.pathsep + env.get('PYTHONPATH', ''),
            'SIDEVOICE_TEST_STORAGE_FLAG': str(storage), 'SIDEVOICE_TEST_LATE_ACK_FLAG': str(late),
            'SIDEVOICE_TEST_SLOW_ACK_FLAG': str(slow)}, storage, late, slow


async def js_register(socket_path, thread):
    deadline = time.monotonic() + 30
    while True:
        try:
            reader, writer = await asyncio.open_unix_connection(str(socket_path))
            break
        except (FileNotFoundError, ConnectionRefusedError):
            if time.monotonic() >= deadline:
                raise
            await asyncio.sleep(.1)
    writer.write((json.dumps({'id': 1, 'method': 'register', 'params': {
        'client_ref': thread, 'harness': 'codex', 'thread': thread, 'title': 'Copied state',
        'delivery': {'kind': 'codex-queue', 'thread': thread}}}) + '\n').encode())
    await writer.drain()
    reply = json.loads(await asyncio.wait_for(reader.readline(), 20))
    assert reply['ok'] and not reply['result']['binding_id'].startswith('local-'), reply
    return reader, writer, reply['result']['binding_id']


async def copied_state():
    """Run v2 JS, v3 Rust, then v2 JS against one Core launch and copied files."""
    binary = Path(os.environ['SIDEVOICE_RUST_PROOF_BIN']).resolve()
    python = Path(os.environ['SIDEVOICE_CORE_PYTHON']).absolute()
    js_cli = Path(__file__).resolve().parents[2] / 'connector' / 'cli.mjs'
    with tempfile.TemporaryDirectory(prefix='sidevoice-rust-copied-') as temporary:
        root = Path(temporary)
        root.chmod(0o700)
        data, codex = root / 'sidevoice', root / 'codex'
        core_data = data / 'core'
        for directory in (data, codex, core_data):
            directory.mkdir(mode=0o700)
        thread = str(uuid.uuid4())
        env = {**os.environ, 'SIDEVOICE_DATA_DIR': str(data), 'CODEX_HOME': str(codex),
               'CODEX_THREAD_ID': thread, 'SIDEVOICE_SERVICE_MANAGER': 'none'}
        core_env, storage_flag, _, slow_flag = core_faults(root, env)
        core_log = (root / 'core.log').open('wb')
        js_log = (root / 'js.log').open('wb')
        rust_log = (root / 'rust.log').open('wb')
        launch = str(uuid.uuid4())
        core = await asyncio.create_subprocess_exec(str(python), '-m', 'sidevoice_core.server',
            '--data-dir', str(core_data), '--socket', str(core_data / 'local.sock'), '--port', '0',
            '--idle-exit', '0', '--launch-id', launch, '--log-file', str(root / 'core-app.log'),
            stdout=core_log, stderr=core_log, env=core_env)
        js = daemon = facade = None
        js_writer = None
        try:
            ready = core_data / 'core.json'
            await until(ready.exists, 'copied-state Core ready', seconds=90)
            assert json.loads(ready.read_text())['launch_id'] == launch
            socket_path = data / 'connector.sock'
            js = await asyncio.create_subprocess_exec('node', str(js_cli), 'connector', '--service',
                stdout=js_log, stderr=js_log, env=env)
            await until(socket_path.exists, 'first JS connector socket')
            js_reader, js_writer, old_id = await js_register(socket_path, thread)
            agents = data / 'agents.json'
            # The service scanner intentionally skips uninstalled profiles. Invoke the
            # same JS store writer directly so the copied fixture is genuinely JS state.
            scan = await asyncio.create_subprocess_exec('node', '--input-type=module', '-e',
                "import {listAgents} from './packages/connector/agents.mjs'; listAgents(process.env, {rescan:true, watch:'codex'});",
                cwd=str(js_cli.parents[2]), env=env, stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE)
            _, scan_error = await asyncio.wait_for(scan.communicate(), 30)
            assert scan.returncode == 0 and agents.exists(), scan_error.decode(errors='replace')[-500:]
            storage_flag.touch()
            js_writer.write((json.dumps({'id': 2, 'method': 'publish', 'params': {
                'client_ref': thread, 'session_id': 'copied-session', 'revision': 1,
                'text': 'JS copied outbox speech'}}) + '\n').encode())
            await js_writer.drain()
            # Core's one injected journal failure leaves the actual JS-created row on disk.
            queued_reply = json.loads(await asyncio.wait_for(js_reader.readline(), 25))
            assert queued_reply['ok'] and queued_reply['result']['status'] == 'queued'
            assert not storage_flag.exists(), 'injected Core journal failure was unused'
            await until(lambda: (data / 'outbox.json').exists() and len(json.loads((data / 'outbox.json').read_text())) == 1,
                        'JS-created queued outbox', seconds=25)
            original = json.loads((data / 'outbox.json').read_text())[0]
            assert original['binding_id'] == old_id and 'client_ref' not in original
            await finish(js)
            js = None
            js_writer.close()
            js_writer = None
            await until(lambda: not socket_path.exists(), 'first JS lock/socket release')
            copied = root / 'js-state'
            copied.mkdir(mode=0o700)
            for name in ('outbox.json', 'agents.json'):
                shutil.copy2(data / name, copied / name)
                (data / name).unlink()
                shutil.copy2(copied / name, data / name)
            agents_before = (data / 'agents.json').read_bytes()
            daemon = await asyncio.create_subprocess_exec(str(binary), 'connector',
                stdout=rust_log, stderr=rust_log, env=env)
            await until(socket_path.exists, 'Rust copied-state socket')
            facade = await asyncio.create_subprocess_exec(str(binary), 'mcp', stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.PIPE, stderr=rust_log, env=env)
            await mcp_request(facade, 'initialize', {'protocolVersion': '2025-06-18', 'capabilities': {},
                'clientInfo': {'name': 'codex', 'version': '0.157.0'}}, 1)
            facade.stdin.write(b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
            await facade.stdin.drain()
            joined = await tool(facade, 'voice_connect', {'title': 'Copied state'}, 2, thread)
            assert joined['binding_id'] == old_id, 'Core must reuse the same-process binding'
            await until(lambda: json.loads((data / 'outbox.json').read_text()) == [], 'copied JS speech admission')
            token = http_json(core_data / 'local.sock', 'POST', '/api/device/local/pair', {'name': 'copied-state'})['token']
            rows = http_json(core_data / 'local.sock', 'GET', f'/api/presentation/history?thread_id={thread}', token=token)['messages']
            assert sum(row['text'] == original['text'] for row in rows) == 1
            assert (data / 'agents.json').read_bytes() == agents_before, 'Rust changed JS agent state'
            await finish(facade)
            facade = None
            await finish(daemon)
            daemon = None
            js = await asyncio.create_subprocess_exec('node', str(js_cli), 'connector', '--service',
                stdout=js_log, stderr=js_log, env=env)
            _, js_writer, again = await js_register(socket_path, thread)
            assert again and json.loads((data / 'outbox.json').read_text()) == []
            rows = http_json(core_data / 'local.sock', 'GET', f'/api/presentation/history?thread_id={thread}', token=token)['messages']
            assert sum(row['text'] == original['text'] for row in rows) == 1
            assert json.loads(agents.read_text())['version'] == 1
            print(json.dumps({'copied_state': 'JS_to_Rust_to_JS', 'core_launch_id': launch,
                              'binding_reused': True, 'speech_once': True, 'agents_readable': True}))
            await finish(js)
            js = None
            js_writer.close()
            js_writer = None
            orphan = {**original, 'event_id': str(uuid.uuid4()), 'utterance_id': str(uuid.uuid4()),
                      'binding_id': str(uuid.uuid4()), 'text': 'Unattributed historical speech'}
            slow_rows = [{**original, 'event_id': str(uuid.uuid4()), 'utterance_id': str(uuid.uuid4()),
                          'binding_id': again, 'text': f'Slow admitted speech {i}'} for i in (1, 2)]
            (data / 'outbox.json').write_text(json.dumps([orphan, *slow_rows]))
            (data / 'outbox.json').chmod(0o600)
            slow_flag.write_text('2')
            daemon = await asyncio.create_subprocess_exec(str(binary), 'connector',
                stdout=rust_log, stderr=rust_log, env=env)
            await until(lambda: json.loads((data / 'proof.json').read_text())['pid'] == daemon.pid,
                        'Rust orphan proof startup')
            await until(socket_path.exists, 'Rust orphan socket')
            facade = await asyncio.create_subprocess_exec(str(binary), 'mcp', stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.PIPE, stderr=rust_log, env=env)
            await mcp_request(facade, 'initialize', {'protocolVersion': '2025-06-18', 'capabilities': {},
                'clientInfo': {'name': 'codex', 'version': '0.157.0'}}, 3)
            facade.stdin.write(b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
            await facade.stdin.drain()
            started = time.monotonic()
            joined = await tool(facade, 'voice_connect', {'title': 'Orphan safety'}, 4, thread)
            registration_seconds = time.monotonic() - started
            assert joined['binding_id'] == again and registration_seconds < 10, (
                'registration waited for two slow speech ACKs')
            await until(lambda: orphan['event_id'] in (root / 'rust.log').read_text()
                        and 'retaining for explicit migration' in (root / 'rust.log').read_text(),
                        'historical orphan diagnostic')
            await until(lambda: json.loads((data / 'outbox.json').read_text()) == [orphan],
                        'owned replay completed after registration', seconds=50)
            rows = http_json(core_data / 'local.sock', 'GET', f'/api/presentation/history?thread_id={thread}', token=token)['messages']
            for speech in slow_rows:
                assert sum(row['text'] == speech['text'] for row in rows) == 1
            print(json.dumps({'registration_seconds': round(registration_seconds, 3),
                              'historical_orphan_retained': True, 'slow_acks': 2}))
        except Exception:
            for handle in (core_log, js_log, rust_log):
                handle.flush()
            for name in ('core.log', 'js.log', 'rust.log', 'connector.log', 'core-app.log'):
                path = root / name
                if path.exists():
                    print(name + ': ' + path.read_text(errors='replace')[-2000:], file=sys.stderr)
            raise
        finally:
            if js_writer:
                js_writer.close()
            for process in (facade, daemon, js, core):
                if process:
                    await finish(process)
            core_log.close()
            js_log.close()
            rust_log.close()


async def exercise():
    binary = Path(os.environ['SIDEVOICE_RUST_PROOF_BIN']).resolve()
    python = Path(os.environ['SIDEVOICE_CORE_PYTHON']).absolute()
    with tempfile.TemporaryDirectory(prefix='sidevoice-rust-core-') as temporary:
        root = Path(temporary)
        root.chmod(0o700)
        data, codex = root / 'sidevoice', root / 'codex'
        core_data = data / 'core'
        rollout_dir = codex / 'sessions' / '2026' / '10' / '03'
        for directory in (data, core_data, codex, rollout_dir, root / 'bin'):
            directory.mkdir(parents=True, exist_ok=True, mode=0o700)
            directory.chmod(0o700)
        thread = str(uuid.uuid4())
        rollout = rollout_dir / f'rollout-test-{thread}.jsonl'
        rollout.touch(mode=0o600)
        queued = root / 'queued.json'
        slow_started = root / 'slow-started'
        fake_codex = root / 'bin' / 'codex'
        fake_codex.write_text('''#!/usr/bin/env python3
import json, os, pathlib, sys, time
assert sys.argv[1:3] == ['queue', '--thread'] and sys.argv[4] == '--message'
if 'SLOW_NO_QUEUE' in sys.argv[5]:
    pathlib.Path(os.environ['SIDEVOICE_TEST_SLOW']).touch()
    time.sleep(5)
pathlib.Path(os.environ['SIDEVOICE_TEST_QUEUE']).write_text(json.dumps({'thread':sys.argv[3], 'message':sys.argv[5]}))
''')
        fake_codex.chmod(0o700)
        env = {**os.environ, 'SIDEVOICE_DATA_DIR': str(data), 'CODEX_HOME': str(codex),
               'CODEX_THREAD_ID': thread, 'SIDEVOICE_CODEX_BIN': str(fake_codex),
               'SIDEVOICE_TEST_QUEUE': str(queued), 'SIDEVOICE_TEST_SLOW': str(slow_started)}
        core_env, storage_flag, late_flag, _ = core_faults(root, env)
        launch_id = str(uuid.uuid4())
        core_log = (root / 'core.log').open('wb')
        daemon_log = (root / 'daemon.log').open('wb')
        async def start_core(identity):
            return await asyncio.create_subprocess_exec(str(python), '-m', 'sidevoice_core.server', '--data-dir', str(core_data),
                '--socket', str(core_data / 'local.sock'), '--port', '0', '--idle-exit', '0', '--launch-id', identity,
                '--log-file', str(root / 'core-app.log'), stdout=core_log, stderr=core_log, env=core_env)
        core = await start_core(launch_id)
        daemon = None
        facade = None
        try:
            ready_path = core_data / 'core.json'
            await until(lambda: ready_path.exists() or core.returncode is not None, 'Core ready file', seconds=90)
            assert ready_path.exists(), f'Core exited before ready: {core.returncode}'
            ready = json.loads(ready_path.read_text())
            assert ready['launch_id'] == launch_id and 3 in ready['connector_protocols']
            daemon = await asyncio.create_subprocess_exec(str(binary), 'connector', stdout=daemon_log, stderr=daemon_log, env=env)
            await until(lambda: (data / 'connector.sock').exists(), 'Rust connector socket')
            evidence = json.loads((data / 'proof.json').read_text())
            assert evidence['pid'] == daemon.pid and evidence['core_launch_id'] == launch_id and evidence['protocol'] == 3
            assert evidence['executable'] == str(binary) and len(evidence['executable_sha256']) == 64
            facade = await asyncio.create_subprocess_exec(str(binary), 'mcp', stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.PIPE, stderr=daemon_log, env=env)
            initialized = await mcp_request(facade, 'initialize', {'protocolVersion': '2025-06-18', 'capabilities': {},
                'clientInfo': {'name': 'codex', 'version': '0.157.0'}}, 1)
            assert initialized['instructions'] and initialized['capabilities']['tools'] == {}
            facade.stdin.write(b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
            await facade.stdin.drain()
            listed = await mcp_request(facade, 'tools/list', {}, 2)
            assert {tool['name'] for tool in listed['tools']} == {'voice_connect', 'voice_pair', 'voice_say', 'voice_disconnect', 'voice_pair_device', 'voice_status'}
            joined = await tool(facade, 'voice_connect', {'title': 'Rust v3 interop'}, 3, thread)
            assert joined['conversation'] == thread and not joined['binding_id'].startswith('local-'), joined
            paired = http_json(core_data / 'local.sock', 'POST', '/api/device/local/pair', {'name': 'rust-v3-test'})
            token = paired['token']
            async with unix_connect(path=str(core_data / 'local.sock'), uri='ws://localhost/api/presentation/ws',
                                    subprotocols=['sidevoice', 'sidevoice.token.' + token]) as ws:
                await ws.send(json.dumps({'label': 'rtvi-ai', 'type': 'client-ready', 'id': 'x',
                                         'data': {'settings': {'turn_end_mode': 'timer'}}}))
                while True:
                    frame = json.loads(await asyncio.wait_for(ws.recv(), 15))
                    if frame.get('type') == 'voice-session':
                        session = frame['data']['session_id']
                        break
                selected = http_json(core_data / 'local.sock', 'POST', '/api/presentation/select',
                    {'session_id': session, 'thread_id': thread}, token)
                message_id = str(uuid.uuid4())
                sent = http_json(core_data / 'local.sock', 'POST', '/api/presentation/text',
                    {'text': 'Core generated this input', 'session_id': session, 'thread_id': thread,
                     'binding_id': selected['binding']['binding_id'], 'message_id': message_id}, token)
                queued_message = await until(lambda: queued.exists() and json.loads(queued.read_text()), 'Codex queue call')
                assert queued_message['thread'] == thread and 'Core generated this input' in queued_message['message']
                history = lambda: http_json(core_data / 'local.sock', 'GET', f'/api/presentation/history?thread_id={thread}', token=token)['messages']
                delivered = await until(lambda: next((row for row in history() if row['id'] == sent['id'] and row['status'] == 'delivered'), None), 'accepted receipt')
                assert delivered['status'] == 'delivered'
                with rollout.open('a') as output:
                    output.write(json.dumps({'type': 'noise', 'payload': 'x' * (1 << 20)}) + '\n')
                    output.write(json.dumps({'type': 'response_item', 'payload': {'type': 'message', 'role': 'user',
                        'content': [{'type': 'input_text', 'text': queued_message['message']}]}}) + '\n')
                await until(lambda: next((row for row in history() if row['id'] == sent['id'] and row['status'] == 'read'), None), 'read receipt')
                for call_id, flag, phrase in ((4, storage_flag, 'Journal failure retains speech'),
                                               (5, late_flag, 'Late ACK replays once')):
                    flag.touch()
                    held = await tool(facade, 'voice_say', {'text': phrase,
                        'session_id': session, 'revision': sent['revision']}, call_id)
                    assert held['status'] == 'queued' and not flag.exists(), held
                    saved = json.loads((data / 'outbox.json').read_text())
                    assert len(saved) == 1 and saved[0]['text'] == phrase
                    utterance = saved[0]['utterance_id']
                    await finish(daemon)
                    daemon = await asyncio.create_subprocess_exec(str(binary), 'connector',
                        stdout=daemon_log, stderr=daemon_log, env=env)
                    await until(lambda: json.loads((data / 'proof.json').read_text())['pid'] == daemon.pid,
                                'same-Core Rust restart proof')
                    await until(lambda: json.loads((data / 'outbox.json').read_text()) == [],
                                'same-Core outbox replay', seconds=60)
                    rows = history()
                    assert sum(row['text'] == phrase for row in rows) == 1
                    assert any(row['id'].endswith(':voice:' + utterance) for row in rows)
                said = await tool(facade, 'voice_say', {'text': 'The Rust proof received your words',
                    'session_id': session, 'revision': sent['revision']}, 6)
                assert said['status'] in {'published', 'queued'}
                if said['status'] == 'published':
                    assert json.loads((data / 'outbox.json').read_text()) == []
                http_json(core_data / 'local.sock', 'POST', '/api/presentation/text',
                    {'text': 'SLOW_NO_QUEUE must die with its Core session', 'session_id': session, 'thread_id': thread,
                     'binding_id': selected['binding']['binding_id'], 'message_id': str(uuid.uuid4())}, token)
                await until(slow_started.exists, 'slow Codex queue process')
                core.kill()
                await core.wait()
                core = None
            await asyncio.sleep(6)
            assert 'SLOW_NO_QUEUE' not in json.loads(queued.read_text())['message'], 'old Core handler queued after disconnect'
            queued_reply = await tool(facade, 'voice_say', {'text': 'Speech queued across a Core restart',
                'session_id': session, 'revision': sent['revision']}, 7)
            assert queued_reply['status'] == 'queued'
            before_restart = json.loads((data / 'outbox.json').read_text())
            assert len(before_restart) == 1 and before_restart[0]['text'] == 'Speech queued across a Core restart'
            await finish(daemon)
            daemon = None
            next_launch = str(uuid.uuid4())
            core = await start_core(next_launch)
            await until(lambda: ready_path.exists() and json.loads(ready_path.read_text())['launch_id'] == next_launch,
                        'restarted Core ready file', seconds=90)
            daemon = await asyncio.create_subprocess_exec(str(binary), 'connector', stdout=daemon_log, stderr=daemon_log, env=env)
            await until(lambda: json.loads((data / 'proof.json').read_text())['core_launch_id'] == next_launch,
                        'restarted Rust proof evidence')
            await until(lambda: json.loads((data / 'outbox.json').read_text()) == [],
                        'outbox replay after Core reminted binding', seconds=60)
            second_token = http_json(core_data / 'local.sock', 'POST', '/api/device/local/pair', {'name': 'rust-v3-replay'})['token']
            replayed = http_json(core_data / 'local.sock', 'GET', f'/api/presentation/history?thread_id={thread}',
                                 token=second_token)['messages']
            assert any(row['role'] == 'assistant' and row['text'] == 'Speech queued across a Core restart'
                       for row in replayed)
            assert not any(row['text'] == 'The Rust proof received your words' for row in replayed), (
                'Core process-only journal unexpectedly survived restart')
            left = await tool(facade, 'voice_disconnect', {}, 8)
            assert left['status'] == 'left'
            print(json.dumps({'core_launch_id': launch_id, 'rust_pid': daemon.pid,
                              'rust_executable_sha256': evidence['executable_sha256'], 'mcp_tools': 6,
                              'core_input': 'accepted_then_read', 'speech': said['status'],
                              'outbox_replayed_after_core_restart': True,
                              'storage_error_retained': True, 'lost_ack_replayed_once': True,
                              'core_restart_after_ack_lost_text': True}))
        except Exception:
            core_log.flush()
            daemon_log.flush()
            print('Core process:', core.returncode, file=sys.stderr)
            for name in ('core.log', 'daemon.log', 'core-app.log'):
                path = root / name
                if path.exists():
                    print(name + ': ' + path.read_text(errors='replace')[-2000:], file=sys.stderr)
            raise
        finally:
            if facade:
                await finish(facade)
            if daemon:
                await finish(daemon)
            if core:
                await finish(core)
            core_log.close()
            daemon_log.close()


if __name__ == '__main__':
    async def main():
        await copied_state()
        await exercise()
    asyncio.run(main())
