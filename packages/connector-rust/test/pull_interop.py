"""Hosted pulled-input test: the Rust Connector against a real Rust Core build.

One conversation joins with input "pull" (a synthetic Claude Code identity whose inbox is
never used) and another with ordinary pushed input (an HTTP receiver). It checks ordered
retrieval, retried fetches, explicit and repeated acknowledgement, isolation, the hook's
mechanical check, push coexistence, and that a disconnect returns unacknowledged input. No
model and no real harness take part.
"""

import asyncio
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import queue
import sys
import tempfile
import threading
import uuid

from interop_core import finish, http_json, mcp_request, until
from websockets.asyncio.client import unix_connect


class Receiver(ThreadingHTTPServer):
    daemon_threads = True

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


async def facade(binary, root, env, client):
    process = await asyncio.create_subprocess_exec(str(binary), '--profile-root', str(root), 'mcp',
        stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL, env=env)
    await mcp_request(process, 'initialize', {'protocolVersion': '2025-06-18', 'capabilities': {},
        'clientInfo': {'name': client, 'version': 'synthetic-no-model-client'}}, 1)
    process.stdin.write(b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
    await process.stdin.drain()
    listed = await mcp_request(process, 'tools/list', {}, 2)
    names = {entry['name'] for entry in listed['tools']}
    assert {'voice_has_pending', 'voice_get_messages'} <= names, names
    return process


class Caller:
    def __init__(self, process):
        self.process, self.serial = process, 10

    async def raw(self, name, args):
        self.serial += 1
        return await mcp_request(self.process, 'tools/call', {'name': name, 'arguments': args}, self.serial)

    async def __call__(self, name, args=None):
        result = await self.raw(name, args or {})
        assert not result.get('isError'), f'{name}: {result}'
        return json.loads(result['content'][0]['text'])


async def hook(binary, root, env, session, tool):
    process = await asyncio.create_subprocess_exec(str(binary), '--profile-root', str(root), 'hook',
        'claude-pre-tool-use', stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE, env=env)
    stdout, stderr = await asyncio.wait_for(process.communicate(json.dumps(
        {'session_id': session, 'hook_event_name': 'PreToolUse', 'tool_name': tool,
         'tool_input': {}}).encode()), 15)
    assert process.returncode == 0, stderr.decode(errors='replace')
    return json.loads(stdout) if stdout.strip() else None


async def exercise():
    binary = Path(os.environ['SIDEVOICE_RUST_PROOF_BIN']).resolve()
    core_binary = Path(os.environ['SIDEVOICE_CORE_RUST_BIN']).resolve()
    with tempfile.TemporaryDirectory(prefix='sidevoice-pull-') as temporary:
        root = Path(temporary)
        root.chmod(0o700)
        home, claude, data, codex = root / 'home', root / 'claude', root / 'sidevoice', root / 'codex'
        core_data = data / 'core'
        for directory in (home, claude, claude / 'sessions', data, core_data, codex, root / 'cursor',
                          root / 'cursor/config', root / 'cursor/data', root / 'xdg', root / 'xdg/config',
                          root / 'xdg/data'):
            directory.mkdir(mode=0o700)
        pull_thread, push_thread = str(uuid.uuid4()), str(uuid.uuid4())
        (claude / 'settings.json').write_text(json.dumps({'permissions': {'defaultMode': 'default'}}))
        (claude / 'settings.json').chmod(0o600)
        (claude / 'sessions' / 'pull.json').write_text(json.dumps(
            {'sessionId': pull_thread, 'pid': os.getpid(), 'status': 'idle'}))
        adapter_keys = {'CLAUDE_CODE_SESSION_ID', 'CLAUDE_CODE_MESSAGING_SOCKET', 'CLAUDE_CODE_MESSAGING_TOKEN',
                        'CODEX_THREAD_ID', 'SIDEVOICE_THREAD', 'SIDEVOICE_DELIVERY_URL', 'SIDEVOICE_HARNESS',
                        'SIDEVOICE_TITLE'}
        env = {**{key: value for key, value in os.environ.items() if key not in adapter_keys},
               'HOME': str(home), 'CLAUDE_CONFIG_DIR': str(claude), 'CODEX_HOME': str(codex),
               'CURSOR_CONFIG_DIR': str(root / 'cursor/config'), 'CURSOR_DATA_DIR': str(root / 'cursor/data'),
               'XDG_CONFIG_HOME': str(root / 'xdg/config'), 'XDG_DATA_HOME': str(root / 'xdg/data'),
               'SIDEVOICE_DATA_DIR': str(data), 'SIDEVOICE_SERVICE_MANAGER': 'none'}
        pull_env = {**env, 'CLAUDE_CODE_SESSION_ID': pull_thread,
                    'CLAUDE_CODE_MESSAGING_SOCKET': str(root / 'unused-inbox.sock'),
                    'CLAUDE_CODE_MESSAGING_TOKEN': 'unused'}
        receiver = Receiver()
        threading.Thread(target=receiver.serve_forever, daemon=True).start()
        push_env = {**env, 'SIDEVOICE_THREAD': push_thread,
                    'SIDEVOICE_DELIVERY_URL': f'http://127.0.0.1:{receiver.server_port}/input'}
        logs = (root / 'core.log').open('wb')
        core = await asyncio.create_subprocess_exec(str(core_binary), '--data-dir', str(core_data),
            '--socket', str(core_data / 'local.sock'), '--port', '0', '--idle-exit', '0',
            '--launch-id', str(uuid.uuid4()), '--log-file', str(root / 'core-app.log'),
            stdout=logs, stderr=logs, env=env)
        daemon = pulled = pushed = rejoined = None
        try:
            await until(lambda: (core_data / 'core.json').exists() or core.returncode is not None,
                        'Rust Core ready file', seconds=90)
            assert core.returncode is None, f'Rust Core exited {core.returncode}'
            daemon = await asyncio.create_subprocess_exec(str(binary), '--profile-root', str(root), 'connector',
                stdout=logs, stderr=logs, env=env)
            await until((data / 'proof.json').exists, 'Rust Connector linked to Core')
            sock = core_data / 'local.sock'

            pulled = Caller(await facade(binary, root, pull_env, 'claude-code'))
            assert (await pulled('voice_has_pending')) == {'connected': False, 'pending': False, 'count': 0}
            joined = await pulled('voice_connect', {'title': 'Pulled input', 'input': 'pull'})
            assert joined['delivery'] == 'pull' and joined['conversation'] == pull_thread, joined
            assert 'voice_in' not in joined and 'ack_ids' in joined['pull'], joined
            pushed = Caller(await facade(binary, root, push_env, 'http-fixture'))
            push_joined = await pushed('voice_connect', {'title': 'Pushed input'})
            assert push_joined['delivery'] == 'push', push_joined
            refused = await pushed.raw('voice_get_messages', {})
            assert refused.get('isError'), 'a pushed conversation must not read by pull'

            token = http_json(sock, 'POST', '/api/device/local/pair', {'name': 'pull-interop'})['token']
            history = lambda thread: http_json(sock, 'GET', f'/api/presentation/history?thread_id={thread}',
                                               token=token)['messages']
            async with unix_connect(path=str(sock), uri='ws://localhost/api/presentation/ws',
                                    subprotocols=['sidevoice', 'sidevoice.token.' + token]) as ws:
                await ws.send(json.dumps({'label': 'rtvi-ai', 'type': 'client-ready', 'id': 'x',
                                          'data': {'settings': {'turn_end_mode': 'timer'}}}))
                while True:
                    frame = json.loads(await asyncio.wait_for(ws.recv(), 15))
                    if frame.get('type') == 'voice-session':
                        session = frame['data']['session_id']
                        break

                def say(thread, text):
                    selected = http_json(sock, 'POST', '/api/presentation/select',
                                         {'session_id': session, 'thread_id': thread}, token)
                    sent = http_json(sock, 'POST', '/api/presentation/text', {
                        'text': text, 'session_id': session, 'thread_id': thread,
                        'binding_id': selected['binding']['binding_id'], 'message_id': str(uuid.uuid4())}, token)
                    assert sent.get('accepted') is True, sent
                    return sent['id']

                async def status(thread, row_id, wanted):
                    await until(lambda: any(row['id'] == row_id and row['status'] == wanted
                                            for row in history(thread)), f'{row_id} {wanted}', seconds=15)

                assert await hook(binary, root, env, pull_thread, 'Bash') is None, 'nothing waits yet'
                first, second = say(pull_thread, 'first pulled words'), say(pull_thread, 'second pulled words')
                await asyncio.sleep(1)  # Core's push pump runs every 250 ms: it must leave them.
                assert {row['status'] for row in history(pull_thread) if row['id'] in (first, second)} == {'pending'}
                check = await pulled('voice_has_pending')
                assert (check['pending'], check['count'], check['unfetched']) == (True, 2, 2), check

                denied = await hook(binary, root, env, pull_thread, 'Bash')
                assert denied['hookSpecificOutput']['permissionDecision'] == 'deny', denied
                assert 'voice_get_messages' in denied['hookSpecificOutput']['permissionDecisionReason']
                assert await hook(binary, root, env, pull_thread, 'mcp__sidevoice__voice_get_messages') is None
                assert await hook(binary, root, env, push_thread, 'Bash') is None, 'push conversation'
                assert await hook(binary, root, env, str(uuid.uuid4()), 'Bash') is None, 'unknown conversation'

                got = await pulled('voice_get_messages')
                assert [m['text'] for m in got['messages']] == ['first pulled words', 'second pulled words'], got
                assert got['more'] is False and got['remaining'] == 2 and got['acknowledged'] == 0, got
                ids = [m['message_id'] for m in got['messages']]
                await status(pull_thread, first, 'delivered')
                assert await hook(binary, root, env, pull_thread, 'Bash') is None, 'no second denial'
                again = await pulled('voice_get_messages')
                assert [m['message_id'] for m in again['messages']] == ids, 'a retried fetch returns the same IDs'

                message = got['messages'][0]
                spoken = await pulled('voice_say', {'text': 'Understood.', 'session_id': message['session_id'],
                                                    'revision': message['revision']})
                assert isinstance(spoken, dict), spoken

                acked = await pulled('voice_get_messages', {'ack_ids': [ids[0]]})
                assert (acked['acknowledged'], acked['remaining']) == (1, 1), acked
                assert [m['message_id'] for m in acked['messages']] == ids[1:], acked
                await status(pull_thread, first, 'read')
                repeated = await pulled('voice_get_messages', {'ack_ids': ids})
                assert (repeated['acknowledged'], repeated['remaining'], repeated['messages']) == (1, 0, []), repeated
                assert (await pulled('voice_has_pending'))['pending'] is False

                pushed_row = say(push_thread, 'pushed words')
                delivered = await asyncio.to_thread(receiver.messages.get, True, 15)
                assert delivered['thread_id'] == push_thread and delivered['text'] == 'pushed words', delivered
                await status(push_thread, pushed_row, 'delivered')
                assert (await pulled('voice_has_pending'))['count'] == 0, 'push input must not reach pull'

                third = say(pull_thread, 'third pulled words')
                held = await pulled('voice_get_messages')
                held_id = held['messages'][0]['message_id']
                await status(pull_thread, third, 'delivered')
                await finish(pulled.process)
                pulled = None
                await status(pull_thread, third, 'pending')
                assert await hook(binary, root, env, pull_thread, 'Bash') is None, 'left: no check'

                rejoined = Caller(await facade(binary, root, pull_env, 'claude-code'))
                await rejoined('voice_connect', {'title': 'Pulled input', 'input': 'pull'})
                returned = await rejoined('voice_get_messages')
                assert [m['message_id'] for m in returned['messages']] == [held_id], returned
                done = await rejoined('voice_get_messages', {'ack_ids': [held_id]})
                assert (done['acknowledged'], done['remaining']) == (1, 0), done
                await status(pull_thread, third, 'read')
            print(json.dumps({'pull_ordered': True, 'retry_same_ids': True, 'ack_idempotent': True,
                              'hook_denied_once': True, 'isolation': True, 'push_coexists': True,
                              'released_on_disconnect': True}))
        except Exception:
            logs.flush()
            for name in ('core.log', 'core-app.log', 'connector.log'):
                path = root / name
                if path.exists():
                    print(name + ': ' + path.read_text(errors='replace')[-3000:], file=sys.stderr)
            raise
        finally:
            for caller in (pulled, pushed, rejoined):
                if caller:
                    await finish(caller.process)
            for process in (daemon, core):
                if process:
                    await finish(process)
            receiver.shutdown()
            logs.close()


if __name__ == '__main__':
    asyncio.run(exercise())
