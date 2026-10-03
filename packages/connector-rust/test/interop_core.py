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


def http_response(socket_path, method, path, body=None, token=None, origin=None):
    connection = UnixHTTP(str(socket_path))
    headers = {'Host': 'localhost'}
    if body is not None:
        headers['Content-Type'] = 'application/json'
    if token:
        headers['Authorization'] = 'Bearer ' + token
    if origin:
        headers['Origin'] = origin
    connection.request(method, path, json.dumps(body).encode() if body is not None else None, headers)
    response = connection.getresponse()
    payload = response.read()
    connection.close()
    result = json.loads(payload) if payload else None
    return response.status, result


def http_json(socket_path, method, path, body=None, token=None):
    status, result = http_response(socket_path, method, path, body, token)
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


def process_alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


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
        home, claude = root / 'home', root / 'claude'
        data, codex, cursor = root / 'sidevoice', root / 'codex', root / 'cursor'
        core_data = data / 'core'
        for directory in (home, claude, codex, cursor, data, core_data):
            directory.mkdir(mode=0o700)
        thread = str(uuid.uuid4())
        proof_args = ['mcp', '--profile-root', str(root)]
        fake_codex = root / 'codex-bin'
        codex_entry = {'name': 'sidevoice', 'transport': {'type': 'stdio',
            'command': str(binary), 'args': proof_args}, 'enabled': True}
        fake_codex.write_text('#!/usr/bin/python3\nimport sys\n'
            "if sys.argv[1:] == ['--version']:\n print('Codex fixture 1')\n"
            "elif sys.argv[1:] == ['mcp', 'get', 'sidevoice', '--json']:\n print(" +
            repr(json.dumps(codex_entry)) + ")\n"
            "else:\n raise SystemExit(2)\n")
        fake_codex.chmod(0o700)
        (codex / 'config.toml').write_text(
            '[mcp_servers.sidevoice]\ncommand = ' + json.dumps(str(binary)) +
            '\nargs = ' + json.dumps(proof_args) + '\n')
        (codex / 'config.toml').chmod(0o600)
        env = {**os.environ, 'HOME': str(home), 'CLAUDE_CONFIG_DIR': str(claude),
               'SIDEVOICE_DATA_DIR': str(data), 'CODEX_HOME': str(codex),
               'CURSOR_CONFIG_DIR': str(cursor),
               'SIDEVOICE_CODEX_BIN': str(fake_codex),
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
                "import {listAgents, agentAction} from './packages/connector/agents.mjs'; "
                "const before=listAgents(process.env, {rescan:true}); "
                "if (before.agents.find(agent => agent.id === 'codex')?.registration !== 'foreign') throw Error('JS must keep the proof-only Codex entry foreign'); "
                "const result = agentAction('dismiss', 'cursor', process.env); "
                "if (!result.agents.find(agent => agent.id === 'cursor')?.dismissed) throw Error('JS dismissal was not saved'); "
                "const fs = await import('node:fs'); "
                "const file = process.env.SIDEVOICE_DATA_DIR + '/agents.json'; "
                "const state = JSON.parse(fs.readFileSync(file, 'utf8')); "
                "state.fixture_unrelated = {owner:'JS fixture', preserved:true}; "
                "fs.writeFileSync(file, JSON.stringify(state)); fs.chmodSync(file, 0o600);",
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
            js_agents_state = json.loads(agents.read_text())
            js_cursor_generation = js_agents_state['seen']['cursor']['generation']
            rust_scan = await asyncio.create_subprocess_exec(str(binary), '--profile-root', str(root),
                'codex', 'inspect', stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE, env=env)
            rust_scan_stdout, rust_scan_stderr = await asyncio.wait_for(rust_scan.communicate(), 30)
            assert rust_scan.returncode == 0, rust_scan_stderr.decode(errors='replace')[-2000:]
            rust_listing = json.loads(rust_scan_stdout)
            rust_cursor = next(agent for agent in rust_listing['agents'] if agent['id'] == 'cursor')
            assert rust_cursor['dismissed'] is True and rust_cursor['actionable'] is False, rust_cursor
            rust_codex = next(agent for agent in rust_listing['agents'] if agent['id'] == 'codex')
            assert rust_codex['registration'] == 'connected', rust_codex
            agents_after_rust = agents.read_bytes()
            rust_written_state = json.loads(agents_after_rust)
            assert rust_written_state['seen']['codex']['agent']['registration'] == 'connected'
            assert rust_written_state['dismissed']['cursor'] == js_cursor_generation
            assert rust_written_state['fixture_unrelated']['preserved'] is True
            assert agents_after_rust != (copied / 'agents.json').read_bytes(), 'Rust did not rescan and rewrite JS host state'
            js_reread = await asyncio.create_subprocess_exec('node', '--input-type=module', '-e',
                "import {listAgents} from './packages/connector/agents.mjs'; "
                "const result=listAgents(process.env, {rescan:true}); "
                "const cursor=result.agents.find(agent => agent.id === 'cursor'); "
                "const codex=result.agents.find(agent => agent.id === 'codex'); "
                "const fs=await import('node:fs'); const state=JSON.parse(fs.readFileSync(process.env.SIDEVOICE_DATA_DIR + '/agents.json','utf8')); "
                "if (!cursor?.dismissed || cursor.actionable || codex?.registration !== 'foreign' || state.fixture_unrelated?.preserved !== true) throw Error('JS rescan did not preserve dismissal and keep the proof-only entry foreign'); "
                "console.log(JSON.stringify({dismissed:cursor.dismissed, generation:state.seen.cursor.generation, codex:codex.registration, unrelated:state.fixture_unrelated}));",
                cwd=str(js_cli.parents[2]), env=env, stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE)
            reread_stdout, reread_stderr = await asyncio.wait_for(js_reread.communicate(), 30)
            assert js_reread.returncode == 0, reread_stderr.decode(errors='replace')[-1000:]
            reread = json.loads(reread_stdout)
            assert reread['generation'] == js_cursor_generation and reread['dismissed'] is True \
                and reread['codex'] == 'foreign', reread
            daemon = await asyncio.create_subprocess_exec(str(binary), 'connector', '--profile-root', str(root),
                stdout=rust_log, stderr=rust_log, env=env)
            await until(socket_path.exists, 'Rust copied-state socket')
            facade = await asyncio.create_subprocess_exec(str(binary), 'mcp', '--profile-root', str(root), stdin=asyncio.subprocess.PIPE,
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
                              'binding_reused': True, 'speech_once': True,
                              'agents_dismissal_preserved': True, 'agents_unknown_field_preserved': True}))
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
            daemon = await asyncio.create_subprocess_exec(str(binary), 'connector', '--profile-root', str(root),
                stdout=rust_log, stderr=rust_log, env=env)
            await until(lambda: json.loads((data / 'proof.json').read_text())['pid'] == daemon.pid,
                        'Rust orphan proof startup')
            await until(socket_path.exists, 'Rust orphan socket')
            facade = await asyncio.create_subprocess_exec(str(binary), 'mcp', '--profile-root', str(root), stdin=asyncio.subprocess.PIPE,
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
        home, claude = root / 'home', root / 'claude'
        data, codex, cursor = root / 'sidevoice', root / 'codex', root / 'cursor'
        core_data = data / 'core'
        rollout_dir = codex / 'sessions' / '2026' / '10' / '03'
        for directory in (home, claude, data, core_data, codex, cursor, rollout_dir, root / 'bin'):
            directory.mkdir(parents=True, exist_ok=True, mode=0o700)
            directory.chmod(0o700)
        fresh = await asyncio.create_subprocess_exec(str(binary), 'mcp', '--profile-root', str(root),
            stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
            env={})
        fresh_info = await mcp_request(fresh, 'initialize', {'protocolVersion': '2025-06-18',
            'capabilities': {}, 'clientInfo': {'name': 'codex', 'version': 'isolated-empty-env'}}, 90)
        assert fresh_info['instructions']
        fresh_tools = await mcp_request(fresh, 'tools/list', {}, 91)
        assert len(fresh_tools['tools']) == 6, 'profile-root must reconstruct the selected command with no inherited profile env'
        await finish(fresh)
        thread = str(uuid.uuid4())
        rollout = rollout_dir / f'rollout-test-{thread}.jsonl'
        rollout.touch(mode=0o600)
        queued = root / 'queued.json'
        slow_started = root / 'slow-started'
        fake_codex = root / 'bin' / 'codex'
        fake_codex.write_text('''#!/usr/bin/env python3
import json, os, pathlib, sys, time
args = sys.argv[1:]
home = pathlib.Path(os.environ['CODEX_HOME'])
entry_file = home / 'fake-sidevoice.json'
config_file = home / 'config.toml'
if args == ['--version']:
    print('codex interop fixture 1')
elif args[:3] == ['mcp', 'get', 'sidevoice']:
    entry = json.loads(entry_file.read_text()) if entry_file.exists() else None
    unknown = home / 'unknown-next-get'
    if unknown.exists():
        unknown.unlink()
        print('unrecognized Codex MCP response')
        raise SystemExit(0)
    slow = home / 'slow-next-get'
    if slow.exists():
        slow.unlink()
        (home / 'slow-get-started').write_text(str(os.getpid()))
        time.sleep(1.2)
    if entry is None:
        print('No such server: sidevoice', file=sys.stderr)
        raise SystemExit(1)
    print(json.dumps(entry))
elif args[:3] == ['mcp', 'remove', 'sidevoice']:
    entry_file.unlink(missing_ok=True)
    config_file.unlink(missing_ok=True)
elif args[:3] == ['mcp', 'add', 'sidevoice'] and '--' in args:
    start = args.index('--') + 1
    command, command_args = args[start], args[start + 1:]
    fail = home / 'fail-next-add'
    if fail.exists():
        fail.unlink()
        print('fixture private stdout secret')
        print('fixture private stderr secret', file=sys.stderr)
        raise SystemExit(17)
    slow = home / 'slow-next-add'
    if slow.exists():
        slow.unlink()
        (home / 'slow-add-started').write_text(str(os.getpid()))
        delay = float((home / 'slow-add-seconds').read_text()) if (home / 'slow-add-seconds').exists() else 1.2
        time.sleep(delay)
    entry = {'name':'sidevoice','transport':{'type':'stdio','command':command,'args':command_args},'enabled':True}
    entry_file.write_text(json.dumps(entry))
    config_file.write_text('[mcp_servers.sidevoice]\\ncommand = ' + json.dumps(command) + '\\nargs = ' + json.dumps(command_args) + '\\n')
    config_file.chmod(0o600)
elif args[:2] == ['queue', '--thread'] and args[3] == '--message':
    if 'SLOW_NO_QUEUE' in args[4]:
        pathlib.Path(os.environ['SIDEVOICE_TEST_SLOW']).touch()
        time.sleep(5)
    pathlib.Path(os.environ['SIDEVOICE_TEST_QUEUE']).write_text(json.dumps({'thread':args[2], 'message':args[4]}))
else:
    print('unsupported fixture command', file=sys.stderr)
    raise SystemExit(2)
''')
        fake_codex.chmod(0o700)
        fake_claude = root / 'bin' / 'claude'
        fake_claude.write_text('''#!/usr/bin/env python3
import json, os, pathlib, sys
args = sys.argv[1:]
state_file = pathlib.Path(os.environ['CLAUDE_CONFIG_DIR']) / 'fake-sidevoice.json'
if args == ['--version']:
    print('claude interop fixture 1')
elif args == ['mcp', 'get', 'sidevoice']:
    if not state_file.exists():
        print('No such MCP server: sidevoice', file=sys.stderr)
        raise SystemExit(1)
    entry = json.loads(state_file.read_text())
    if entry.get('unrecognized'):
        print('unrecognized Claude MCP response')
        raise SystemExit(0)
    print('Name: sidevoice')
    print('Scope: ' + entry.get('scope', 'user'))
    print('Command: ' + entry['command'])
    print('Args: ' + ' '.join(entry['args']))
elif args == ['mcp', 'remove', '--scope', 'user', 'sidevoice']:
    state_file.unlink(missing_ok=True)
elif args[:5] == ['mcp', 'add', '--scope', 'user', 'sidevoice'] and '--' in args:
    start = args.index('--') + 1
    entry = {'command':args[start], 'args':args[start + 1:]}
    state_file.write_text(json.dumps(entry))
    state_file.chmod(0o600)
else:
    print('unsupported fixture command', file=sys.stderr)
    raise SystemExit(2)
''')
        fake_claude.chmod(0o700)
        env = {**os.environ, 'HOME': str(home), 'CLAUDE_CONFIG_DIR': str(claude),
               'SIDEVOICE_DATA_DIR': str(data), 'CODEX_HOME': str(codex),
               'CURSOR_CONFIG_DIR': str(cursor),
               'CODEX_THREAD_ID': thread, 'SIDEVOICE_CODEX_BIN': str(fake_codex),
               'SIDEVOICE_CLAUDE_BIN': str(fake_claude),
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
        second_facade = None
        try:
            ready_path = core_data / 'core.json'
            await until(lambda: ready_path.exists() or core.returncode is not None, 'Core ready file', seconds=90)
            assert ready_path.exists(), f'Core exited before ready: {core.returncode}'
            ready = json.loads(ready_path.read_text())
            assert ready['launch_id'] == launch_id and 3 in ready['connector_protocols']
            daemon = await asyncio.create_subprocess_exec(str(binary), 'connector', '--profile-root', str(root), stdout=daemon_log, stderr=daemon_log, env=env)
            await until(lambda: (data / 'connector.sock').exists(), 'Rust connector socket')
            evidence = json.loads((data / 'proof.json').read_text())
            assert evidence['pid'] == daemon.pid and evidence['core_launch_id'] == launch_id and evidence['protocol'] == 3
            assert evidence['executable'] == str(binary) and len(evidence['executable_sha256']) == 64
            facade = await asyncio.create_subprocess_exec(str(binary), 'mcp', '--profile-root', str(root), stdin=asyncio.subprocess.PIPE,
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
            default_agents = http_json(core_data / 'local.sock', 'GET', '/api/host/agents', token=token)
            assert {agent['id'] for agent in default_agents['agents']} == {'claude', 'codex', 'cursor'}
            assert isinstance(default_agents['scanned_at'], str) and default_agents['custom']['snippet']
            rescanned = http_json(core_data / 'local.sock', 'GET', '/api/host/agents?rescan=1', token=token)
            watched = http_json(core_data / 'local.sock', 'GET', '/api/host/agents?rescan=0&watch=codex', token=token)
            assert {agent['id'] for agent in rescanned['agents']} == {'claude', 'codex', 'cursor'}
            assert next(agent for agent in watched['agents'] if agent['id'] == 'codex')['id'] == 'codex'
            unauth_status, _ = http_response(core_data / 'local.sock', 'GET', '/api/host/agents')
            origin_status, _ = http_response(core_data / 'local.sock', 'GET', '/api/host/agents', token=token,
                                             origin='https://untrusted.invalid')
            malformed_status, malformed = http_response(core_data / 'local.sock', 'GET',
                '/api/host/agents?rescan=maybe', token=token)
            assert unauth_status in (401, 403) and origin_status == 403
            assert malformed_status == 400 and malformed['key'] == 'invalid-rescan'
            unknown_status, unknown = http_response(core_data / 'local.sock', 'POST',
                '/api/host/agents/unknown/connect', token=token)
            assert unknown_status == 409 and unknown['error']['key'] == 'agents.unknown'

            proof_command = str(binary)
            proof_args = ['mcp', '--profile-root', str(root)]
            disabled_entry = {'name':'sidevoice','transport':{'type':'stdio','command':proof_command,
                'args':proof_args},'enabled':False}
            codex_entry = codex / 'fake-sidevoice.json'
            codex_entry.write_text(json.dumps(disabled_entry))
            codex_entry.chmod(0o600)
            (codex / 'config.toml').write_text('[mcp_servers.sidevoice]\ncommand = ' + json.dumps(proof_command)
                + '\nargs = ' + json.dumps(proof_args) + '\nenabled = false\n')
            (codex / 'config.toml').chmod(0o600)
            disabled = http_json(core_data / 'local.sock', 'GET',
                '/api/host/agents?rescan=1&watch=codex', token=token)
            assert next(agent for agent in disabled['agents'] if agent['id'] == 'codex')['registration'] == 'not-connected'
            reenabled = http_json(core_data / 'local.sock', 'POST', '/api/host/agents/codex/connect', token=token)
            assert next(agent for agent in reenabled['agents'] if agent['id'] == 'codex')['registration'] == 'connected'

            http_json(core_data / 'local.sock', 'POST', '/api/host/agents/codex/disconnect', token=token)
            (codex / 'fail-next-add').touch()
            failure_status, failure = http_response(core_data / 'local.sock', 'POST',
                '/api/host/agents/codex/connect', token=token)
            assert failure_status == 409 and failure['error']['key'] == 'agents.action-failed'
            assert 'fixture private' not in json.dumps(failure), 'raw CLI output escaped the keyed Core response'

            (codex / 'unknown-next-get').touch()
            unknown_cli_status, unknown_cli = http_response(core_data / 'local.sock', 'POST',
                '/api/host/agents/codex/connect', token=token)
            assert unknown_cli_status == 409 and unknown_cli['error']['key'] == 'agents.registration-unknown'

            foreign_entry = {'name':'sidevoice','transport':{'type':'stdio','command':'/tmp/foreign-codex',
                'args':['--keep-my-settings']},'enabled':True}
            codex_entry.write_text(json.dumps(foreign_entry))
            codex_entry.chmod(0o600)
            codex_config = codex / 'config.toml'
            codex_config.write_text('[mcp_servers.sidevoice]\ncommand = "/tmp/foreign-codex"\nargs = ["--keep-my-settings"]\n')
            codex_config.chmod(0o600)
            foreign_before = (codex_entry.read_bytes(), codex_config.read_bytes())
            foreign_status, foreign = http_response(core_data / 'local.sock', 'POST',
                '/api/host/agents/codex/connect', token=token)
            assert foreign_status == 409 and foreign['error']['key'] == 'agents.foreign'
            assert (codex_entry.read_bytes(), codex_config.read_bytes()) == foreign_before
            codex_entry.unlink()
            codex_config.unlink()
            codex_config.write_text('mcp_servers.sidevoice = [invalid TOML\n')
            codex_config.chmod(0o600)
            invalid_before = codex_config.read_bytes()
            invalid_status, invalid = http_response(core_data / 'local.sock', 'POST',
                '/api/host/agents/codex/connect', token=token)
            assert invalid_status == 409 and invalid['error']['key'] == 'agents.invalid'
            assert codex_config.read_bytes() == invalid_before
            codex_config.unlink()

            claude_entry = claude / 'fake-sidevoice.json'
            claude_entry.write_text(json.dumps({'scope':'project','command':'/tmp/foreign-claude','args':['mcp']}))
            claude_entry.chmod(0o600)
            claude_before = claude_entry.read_bytes()
            foreign_status, foreign = http_response(core_data / 'local.sock', 'POST',
                '/api/host/agents/claude/connect', token=token)
            assert foreign_status == 409 and foreign['error']['key'] == 'agents.foreign'
            assert claude_entry.read_bytes() == claude_before
            claude_entry.write_text(json.dumps({'unrecognized':True}))
            claude_entry.chmod(0o600)
            unknown_status, unknown = http_response(core_data / 'local.sock', 'POST',
                '/api/host/agents/claude/connect', token=token)
            assert unknown_status == 409 and unknown['error']['key'] == 'agents.registration-unknown'
            claude_entry.unlink()

            for agent_id in ('claude', 'codex', 'cursor'):
                path = f'/api/host/agents/{agent_id}'
                connected = http_json(core_data / 'local.sock', 'POST', path + '/connect', token=token)
                assert next(agent for agent in connected['agents'] if agent['id'] == agent_id)['registration'] == 'connected'
                disconnected = http_json(core_data / 'local.sock', 'POST', path + '/disconnect', token=token)
                assert next(agent for agent in disconnected['agents'] if agent['id'] == agent_id)['registration'] == 'not-connected'
                dismissed = http_json(core_data / 'local.sock', 'POST', path + '/dismiss', token=token)
                dismissed_row = next(agent for agent in dismissed['agents'] if agent['id'] == agent_id)
                assert dismissed_row['dismissed'] and not dismissed_row['actionable']
                connected = http_json(core_data / 'local.sock', 'POST', path + '/connect', token=token)
                connected_row = next(agent for agent in connected['agents'] if agent['id'] == agent_id)
                assert connected_row['registration'] == 'connected' and not connected_row['dismissed']
                disconnected = http_json(core_data / 'local.sock', 'POST', path + '/disconnect', token=token)
                assert next(agent for agent in disconnected['agents'] if agent['id'] == agent_id)['registration'] == 'not-connected'

            second_thread = str(uuid.uuid4())
            second_env = {**env, 'CODEX_THREAD_ID': second_thread}
            second_facade = await asyncio.create_subprocess_exec(str(binary), 'mcp', '--profile-root', str(root),
                stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=daemon_log, env=second_env)
            await mcp_request(second_facade, 'initialize', {'protocolVersion': '2025-06-18', 'capabilities': {},
                'clientInfo': {'name': 'codex', 'version': '0.157.0'}}, 91)
            second_facade.stdin.write(b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
            await second_facade.stdin.drain()

            (codex / 'slow-add-seconds').write_text('4.0')
            (codex / 'slow-next-add').touch()
            (codex / 'slow-add-started').unlink(missing_ok=True)
            timeout_started = time.monotonic()
            timeout_status, timeout_error = http_response(core_data / 'local.sock', 'POST',
                '/api/host/agents/codex/connect', token=token)
            assert timeout_status == 409 and timeout_error['error']['key'] == 'agents.action-failed'
            assert time.monotonic() - timeout_started < 6 and not codex_entry.exists()
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
                (codex / 'slow-next-get').touch()
                (codex / 'slow-get-started').unlink(missing_ok=True)
                agent_scan = asyncio.create_task(asyncio.to_thread(http_json, core_data / 'local.sock',
                    'GET', '/api/host/agents?rescan=1&watch=codex', None, token))
                await until((codex / 'slow-get-started').exists, 'slow host-agent scan')
                unrelated_text = 'Core input while Rust inspects Codex'
                unrelated_message_id = str(uuid.uuid4())
                input_task = asyncio.create_task(asyncio.to_thread(http_json, core_data / 'local.sock',
                    'POST', '/api/presentation/text', {'text': unrelated_text, 'session_id': session,
                        'thread_id': thread, 'binding_id': selected['binding']['binding_id'],
                        'message_id': unrelated_message_id}, token))
                registration_task = asyncio.create_task(tool(second_facade, 'voice_connect',
                    {'title': 'Concurrent link reply'}, 92))
                concurrent_failure = None
                try:
                    registration_live = await asyncio.wait_for(asyncio.shield(registration_task), 3)
                    assert registration_live['conversation'] == second_thread \
                        and not registration_live['binding_id'].startswith('local-'), registration_live
                    assert not agent_scan.done(), 'host scan completed before the independent link reply'
                    unrelated_live = await asyncio.wait_for(asyncio.shield(input_task), 3)
                    assert unrelated_live['accepted'] is True
                    await until(lambda: queued.exists() and json.loads(queued.read_text()).get('message', '').find(unrelated_text) >= 0,
                                'unrelated input.deliver while host scan runs', seconds=3)
                    await until(lambda: next((row for row in history()
                        if row['id'] == unrelated_live['id'] and row['status'] == 'delivered'), None),
                        'Core accepted the concurrent input.deliver', seconds=3)
                    assert not agent_scan.done(), 'host scan completed before input.deliver was acknowledged'
                except Exception as error:
                    concurrent_failure = error
                registration, unrelated, scanned_during_input = await asyncio.gather(
                    registration_task, input_task, agent_scan, return_exceptions=True)
                if concurrent_failure is not None:
                    raise AssertionError(f'concurrent Core link check failed: {concurrent_failure!r}') from concurrent_failure
                for label, result in (('second voice_connect', registration),
                                      ('concurrent presentation input', unrelated),
                                      ('concurrent host scan', scanned_during_input)):
                    if isinstance(result, BaseException):
                        raise AssertionError(f'{label} failed during the concurrent link check: {result!r}') from result
                assert registration['conversation'] == second_thread and not registration['binding_id'].startswith('local-'), registration
                assert unrelated['accepted'] is True
                assert any(agent['id'] == 'codex' for agent in scanned_during_input['agents'])
                await finish(second_facade)
                second_facade = None
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
                    daemon = await asyncio.create_subprocess_exec(str(binary), 'connector', '--profile-root', str(root),
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
                (codex / 'slow-add-seconds').write_text('2.0')
                (codex / 'slow-next-add').touch()
                (codex / 'slow-add-started').unlink(missing_ok=True)
                abandoned_host_action = asyncio.create_task(asyncio.to_thread(http_response,
                    core_data / 'local.sock', 'POST', '/api/host/agents/codex/connect', None, token))
                await until((codex / 'slow-add-started').exists, 'delayed Codex add over Core host API')
                abandoned_pid = int((codex / 'slow-add-started').read_text())
                core.kill()
                await core.wait()
                core = None
                abandoned_reply = await asyncio.gather(abandoned_host_action, return_exceptions=True)
                assert isinstance(abandoned_reply[0], BaseException), 'Core disconnect should drop the in-flight host reply'
                await until(lambda: not process_alive(abandoned_pid), 'cancelled Codex CLI reaped', seconds=4)
            await asyncio.sleep(6)
            assert 'SLOW_NO_QUEUE' not in json.loads(queued.read_text())['message'], 'old Core handler queued after disconnect'
            assert not codex_entry.exists(), 'cancelled Codex add wrote after its Core link disappeared'
            assert not process_alive(abandoned_pid), 'cancelled Codex process remained alive after reconnect delay'
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
            daemon = await asyncio.create_subprocess_exec(str(binary), 'connector', '--profile-root', str(root), stdout=daemon_log, stderr=daemon_log, env=env)
            await until(lambda: json.loads((data / 'proof.json').read_text())['core_launch_id'] == next_launch,
                        'restarted Rust proof evidence')
            await until(lambda: json.loads((data / 'outbox.json').read_text()) == [],
                        'outbox replay after Core reminted binding', seconds=60)
            second_token = http_json(core_data / 'local.sock', 'POST', '/api/device/local/pair', {'name': 'rust-v3-replay'})['token']
            observed = http_json(core_data / 'local.sock', 'GET',
                '/api/host/agents?rescan=1&watch=codex', token=second_token)
            assert next(agent for agent in observed['agents'] if agent['id'] == 'codex')['registration'] == 'not-connected'
            recovered = http_json(core_data / 'local.sock', 'POST',
                '/api/host/agents/codex/connect', token=second_token)
            assert next(agent for agent in recovered['agents'] if agent['id'] == 'codex')['registration'] == 'connected'
            http_json(core_data / 'local.sock', 'POST', '/api/host/agents/codex/disconnect', token=second_token)
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
                              'concurrent_link_reply_and_delivery_before_host_scan': True,
                              'core_input': 'accepted_then_read', 'speech': said['status'],
                              'outbox_replayed_after_core_restart': True,
                              'storage_error_retained': True, 'lost_ack_replayed_once': True,
                              'core_restart_after_ack_lost_text': True}))
        except Exception:
            core_log.flush()
            daemon_log.flush()
            for name, process in (('Core', core), ('Rust daemon', daemon), ('MCP façade', facade),
                                  ('Second MCP façade', second_facade)):
                print(f'{name} returncode: {process.returncode if process else None}', file=sys.stderr)
            proof_path = data / 'proof.json'
            if proof_path.exists():
                print('Rust proof evidence: ' + proof_path.read_text(errors='replace'), file=sys.stderr)
            if facade and facade.returncode is None:
                try:
                    status = await asyncio.wait_for(tool(facade, 'voice_status', {}, 99), 2)
                    print('Rust voice_status: ' + json.dumps(status), file=sys.stderr)
                except Exception as status_error:
                    print(f'Rust voice_status failed: {status_error!r}', file=sys.stderr)
            for name in ('core.log', 'daemon.log', 'core-app.log'):
                path = root / name
                if path.exists():
                    print(name + ':\n' + path.read_text(errors='replace'), file=sys.stderr)
            raise
        finally:
            if facade:
                await finish(facade)
            if second_facade:
                await finish(second_facade)
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
