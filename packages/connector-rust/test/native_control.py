"""Exercise native control in disposable state against the real hosted user manager.

All binaries are built by the same hosted candidate. No operator installation,
provider traffic or published release is involved.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time


def run(binary, args, env, timeout=300, success=True):
    output = subprocess.run([str(binary), *args], env=env, capture_output=True, text=True, timeout=timeout)
    if success and output.returncode:
        logs = []
        for path in Path(env['SIDEVOICE_DATA_DIR']).glob('*.log'):
            logs.append(f'{path.name}: {path.read_text(errors="replace")[-2000:]}')
        raise AssertionError(f'{args}: {output.returncode}\n{output.stdout[-3000:]}\n{output.stderr[-3000:]}\n{logs}')
    answer = json.loads(output.stdout)
    if success:
        assert answer.get('ok') is not False and 'error' not in answer, answer
    return answer, output


def ipc(data, method):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(15)
        connection.connect(str(data / 'connector.sock'))
        connection.sendall(json.dumps({'id': 1, 'method': method, 'params': {}}).encode() + b'\n')
        with connection.makefile('rb') as source:
            answer = json.loads(source.readline(1024 * 1024))
    assert answer['ok'] is True, answer
    return answer['result']


def ready(data):
    return json.loads((data / 'core/core.json').read_bytes())


def selected(root):
    return json.loads((root / 'current/release.json').read_bytes())


def wait_running(binary, env):
    deadline = time.monotonic() + 70
    while time.monotonic() < deadline:
        result, _ = run(binary, ['service', 'status', '--json'], env)
        if result['state'] == 'running':
            return result
        time.sleep(.2)
    raise AssertionError(result)


def main(first, second):
    if os.environ.get('GITHUB_ACTIONS') != 'true':
        raise SystemExit('native lifecycle proof runs only on a disposable GitHub Actions runner')
    with tempfile.TemporaryDirectory(prefix='svnc-', dir='/tmp') as directory:
        base = Path(directory).resolve()
        home = base / 'home'
        data = home / '.sidevoice'
        xdg = home / 'data'
        config = home / 'config'
        # systemd resolves user units using its own login environment, not a client's HOME.
        if os.uname().sysname == 'Linux':
            config = Path(os.environ.get('XDG_CONFIG_HOME', str(Path(os.environ['HOME']) / '.config'))).resolve()
            for unit in ['sidevoice-core.service', 'sidevoice-connector.service']:
                assert not (config / 'systemd/user' / unit).exists(), 'runner has an existing Sidevoice unit'
        for path in [home, data, xdg, config]:
            path.mkdir(mode=0o700, exist_ok=True)
        env = {key: value for key, value in os.environ.items() if not key.startswith('SIDEVOICE_')}
        for key in ['CURSOR_CONFIG_DIR', 'CLAUDE_CONFIG_DIR', 'CODEX_HOME']:
            env.pop(key, None)
        env.update(HOME=str(home), XDG_DATA_HOME=str(xdg), XDG_CONFIG_HOME=str(config),
                   SIDEVOICE_DATA_DIR=str(data), SIDEVOICE_CORE_PORT='0')
        # Cursor's registration is file-based; this stub supplies discovery/version only.
        bin_dir = home / '.local/bin'
        bin_dir.mkdir(parents=True, mode=0o700)
        cursor_cli = bin_dir / 'cursor'
        cursor_cli.write_text('#!/bin/sh\n[ "$1" = "--version" ] || exit 2\nprintf "cursor-fixture 1.0\\n"\n')
        cursor_cli.chmod(0o700)
        root = xdg / 'sidevoice'
        try:
            initial, output = run(first, ['install', '--no-agents', '--json', '--progress=jsonl'], env)
            assert initial['service'] == 'none', initial
            for line in output.stderr.splitlines():
                record = json.loads(line)
                assert record['type'] == 'progress' and set(record) == {'type', 'step', 'done', 'total'}
            release = selected(root)
            assert release['format'] == 'rust-native'
            public = root / 'current/dist/sidevoice'
            runtime = root / 'current/dist/sidevoice-rust'
            assert os.stat(public).st_ino == os.stat(runtime).st_ino, 'duplicate native payload'
            assert hashlib.sha256(public.read_bytes()).hexdigest() == release['runtime_sha256']
            assert not any(path.suffix in {'.js', '.mjs', '.cjs'} for path in (root / 'current').rglob('*'))
            run(first, ['agents', 'connect', 'cursor', '--json'], env)
            cursor = home / '.cursor/mcp.json'
            config_value = json.loads(cursor.read_bytes())
            assert config_value['mcpServers']['sidevoice']['args'][-1] == 'mcp'
            config_value['mcpServers']['foreign-sentinel'] = {'command': '/user/owned/server'}
            cursor.write_text(json.dumps(config_value))
            cursor.chmod(0o600)
            run(first, ['agents', '--json'], env)
            old = ready(data)
            run(first, ['install', '--no-agents', '--json'], env)
            assert ready(data)['launch_id'] == old['launch_id'], 'no-op restarted Core'
            run(first, ['install', '--service', '--no-agents', '--json'], env)
            managed = wait_running(first, env)
            assert ready(data)['launch_id'] != old['launch_id'], 'mode conversion kept unmanaged Core'
            assert ipc(data, 'identity').get('managed') is True, 'mode conversion kept unmanaged Connector'
            before = ready(data)
            run(first, ['install', '--no-agents', '--json'], env)
            assert ready(data)['launch_id'] == before['launch_id'], 'managed no-op restarted Core'
            installed, _ = run(second, ['install', '--no-agents', '--json'], env)
            assert installed['action'] != 'noop', installed
            assert selected(root)['id'] != release['id']
            run(second, ['rollback', '--json'], env)
            assert selected(root)['id'] == release['id']
            wait_running(first, env)
            registered = json.loads(cursor.read_bytes())['mcpServers']['sidevoice']
            assert Path(registered['command']).is_file(), 'rollback pruned a registered command'
            run(first, ['service', 'stop', '--json'], env)
            status, _ = run(first, ['service', 'status', '--json'], env)
            assert status['state'] == 'stopped-by-person', status
            run(first, ['service', 'start', '--json'], env)
            wait_running(first, env)
            before = ready(data)
            run(first, ['service', 'restart', '--json'], env)
            wait_running(first, env)
            assert ready(data)['launch_id'] != before['launch_id']
            print(json.dumps({'ok': True, 'target': os.uname().machine, 'manager': managed['service'],
                              'initial': release['id'], 'update': installed['installed'],
                              'checked': ['fresh', 'noop', 'mode-conversion', 'update', 'rollback',
                                          'stop', 'start', 'restart', 'native-shipping-closure', 'agent-registration']}))
        finally:
            run(second, ['uninstall', '--json'], env, success=False)
        saved = json.loads(cursor.read_bytes())['mcpServers']
        assert 'sidevoice' not in saved and saved['foreign-sentinel']['command'] == '/user/owned/server'
        assert not (root / 'current').exists(), 'uninstall retained selection'
        assert not (data / 'core/core.json').exists(), 'uninstall retained Core state'


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--first', required=True, type=Path)
    parser.add_argument('--second', required=True, type=Path)
    args = parser.parse_args()
    main(args.first.resolve(), args.second.resolve())
