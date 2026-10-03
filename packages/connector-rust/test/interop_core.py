"""Hosted real-Core/v3 test with a deliberately modelled Codex queue.

This checks Core-originated input, durable Core receipts, the Rust façade and
outbox. It does not claim a real authenticated Codex session; that is a
separate acceptance gate.
"""

import asyncio
import http.client
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
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


async def exercise():
    binary = Path(os.environ['SIDEVOICE_RUST_PROOF_BIN']).resolve()
    python = Path(os.environ['SIDEVOICE_CORE_PYTHON']).resolve()
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
        fake_codex = root / 'bin' / 'codex'
        fake_codex.write_text('''#!/usr/bin/env python3
import json, os, pathlib, sys
assert sys.argv[1:3] == ['queue', '--thread'] and sys.argv[4] == '--message'
pathlib.Path(os.environ['SIDEVOICE_TEST_QUEUE']).write_text(json.dumps({'thread':sys.argv[3], 'message':sys.argv[5]}))
''')
        fake_codex.chmod(0o700)
        env = {**os.environ, 'SIDEVOICE_DATA_DIR': str(data), 'CODEX_HOME': str(codex),
               'SIDEVOICE_CODEX_BIN': str(fake_codex), 'SIDEVOICE_TEST_QUEUE': str(queued)}
        launch_id = str(uuid.uuid4())
        core_log = (root / 'core.log').open('wb')
        daemon_log = (root / 'daemon.log').open('wb')
        core = await asyncio.create_subprocess_exec(str(python), '-m', 'sidevoice_core.server', '--data-dir', str(core_data),
            '--socket', str(core_data / 'local.sock'), '--port', '0', '--idle-exit', '0', '--launch-id', launch_id,
            stdout=core_log, stderr=core_log, env=env)
        daemon = None
        facade = None
        try:
            ready_path = core_data / 'core.json'
            await until(lambda: ready_path.exists(), 'Core ready file')
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
            assert joined['conversation'] == thread and not joined['binding_id'].startswith('local-')
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
                http_json(core_data / 'local.sock', 'POST', '/api/presentation/select',
                    {'session_id': session, 'thread_id': thread}, token)
                message_id = str(uuid.uuid4())
                sent = http_json(core_data / 'local.sock', 'POST', '/api/presentation/text',
                    {'text': 'Core generated this input', 'session_id': session, 'thread_id': thread,
                     'binding_id': joined['binding_id'], 'message_id': message_id}, token)
                queued_message = await until(lambda: queued.exists() and json.loads(queued.read_text()), 'Codex queue call')
                assert queued_message['thread'] == thread and 'Core generated this input' in queued_message['message']
                history = lambda: http_json(core_data / 'local.sock', 'GET', f'/api/presentation/history?thread_id={thread}', token=token)['messages']
                delivered = await until(lambda: next((row for row in history() if row['id'] == sent['id'] and row['status'] == 'delivered'), None), 'accepted receipt')
                assert delivered['status'] == 'delivered'
                with rollout.open('a') as output:
                    output.write(json.dumps({'type': 'response_item', 'payload': {'type': 'message', 'role': 'user',
                        'content': [{'type': 'input_text', 'text': queued_message['message']}]}}) + '\n')
                await until(lambda: next((row for row in history() if row['id'] == sent['id'] and row['status'] == 'read'), None), 'read receipt')
                said = await tool(facade, 'voice_say', {'text': 'The Rust proof received your words',
                    'session_id': session, 'revision': sent['revision']}, 4)
                assert said['status'] in {'published', 'queued'}
                if said['status'] == 'published':
                    assert json.loads((data / 'outbox.json').read_text()) == []
                left = await tool(facade, 'voice_disconnect', {}, 5)
                assert left['status'] == 'left'
            print(json.dumps({'core_launch_id': launch_id, 'rust_pid': daemon.pid,
                              'rust_executable_sha256': evidence['executable_sha256'], 'mcp_tools': 6,
                              'core_input': 'accepted_then_read', 'speech': said['status']}))
        except Exception:
            print('Core log tail:', core_log.name, file=sys.stderr)
            print('Daemon log:', daemon_log.name, file=sys.stderr)
            raise
        finally:
            if facade:
                await finish(facade)
            if daemon:
                await finish(daemon)
            await finish(core)
            core_log.close()
            daemon_log.close()


if __name__ == '__main__':
    asyncio.run(exercise())
