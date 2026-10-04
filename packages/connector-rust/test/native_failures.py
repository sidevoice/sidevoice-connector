"""Prove native transaction failure recovery on a disposable GitHub Actions runner."""
import argparse
from contextlib import contextmanager
import fcntl
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time

from native_control import ready, run, selected, wait_running


def wait_for(predicate, child, description, timeout=60):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        if child.poll() is not None:
            raise AssertionError(f'installer exited {child.returncode} before {description}')
        time.sleep(.02)
    raise AssertionError(f'timed out waiting for {description}')


def progress_seen(path, step):
    for line in path.read_text().splitlines():
        try:
            record = json.loads(line)
        except json.JSONDecodeError:
            continue  # The writer may not have completed the last line yet.
        if record.get('type') == 'progress' and record.get('step') == step:
            return True
    return False


@contextmanager
def installer(binary, env, base, name):
    stdout = base / f'{name}.stdout'
    stderr = base / f'{name}.stderr'
    with stdout.open('w') as out, stderr.open('w') as err:
        child = subprocess.Popen([str(binary), 'install', '--no-agents', '--json',
                                  '--progress=jsonl'], env=env, stdout=out, stderr=err,
                                 start_new_session=True)
        try:
            yield child, stdout, stderr
        finally:
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGCONT)
                os.killpg(child.pid, signal.SIGKILL)
            child.wait()


def result(child, stdout, stderr):
    child.wait(timeout=300)
    try:
        return json.loads(stdout.read_text())
    except json.JSONDecodeError as error:
        raise AssertionError(f'{child.returncode}: {stdout.read_text()}\n{stderr.read_text()}') from error


def pointers(root):
    return {name: os.readlink(root / name) if (root / name).is_symlink() else None
            for name in ['current', 'previous', 'verified']}


def manager_gate(base, root, original):
    """Pause a real manager command before it starts the committed candidate."""
    manager = '/bin/launchctl' if sys.platform == 'darwin' else shutil.which('systemctl')
    assert manager, 'systemctl is unavailable'
    script = base / 'manager-gate'
    config = {'root': str(root), 'original': original, 'manager': manager,
              'arrived': str(base / 'manager-arrived'), 'release': str(base / 'manager-release')}
    script.write_text(f'#!{sys.executable}\n' + 'CONFIG = ' + repr(config) + '\n' + r'''
import json, os, pathlib, sys, time
root = pathlib.Path(CONFIG['root'])
arrived = pathlib.Path(CONFIG['arrived'])
release = pathlib.Path(CONFIG['release'])
args = sys.argv[1:]
selection = json.loads((root / 'current/release.json').read_bytes())
is_start = ('bootstrap' in args and any(arg.endswith('/dev.sidevoice.core.plist') for arg in args)) or (
    any(arg in ('start', 'restart') for arg in args) and 'sidevoice-core.service' in args)
if is_start and selection['id'] != CONFIG['original'] and not release.exists():
    arrived.write_text(json.dumps({'pid': os.getpid(), 'selected': selection['id'], 'args': args}))
    end = time.monotonic() + 25
    while not release.exists():
        if time.monotonic() >= end:
            sys.exit(90)
        time.sleep(.01)
os.execv(CONFIG['manager'], [CONFIG['manager'], *args])
''')
    script.chmod(0o700)
    return script, Path(config['arrived']), Path(config['release'])


def main(first, second):
    if os.environ.get('GITHUB_ACTIONS') != 'true':
        raise SystemExit('native failure proof runs only on a disposable GitHub Actions runner')
    with tempfile.TemporaryDirectory(prefix='svnf-', dir='/tmp') as directory:
        base = Path(directory).resolve()
        home, data, xdg = base / 'home', base / 'home/.sidevoice', base / 'home/data'
        config = home / 'config'
        if sys.platform == 'linux':
            config = Path(os.environ.get('XDG_CONFIG_HOME', str(Path(os.environ['HOME']) / '.config'))).resolve()
            for unit in ['sidevoice-core.service', 'sidevoice-connector.service']:
                assert not (config / 'systemd/user' / unit).exists(), 'existing runner Sidevoice unit'
        for path in [home, data, xdg, config]:
            path.mkdir(mode=0o700, parents=True, exist_ok=True)
        env = {key: value for key, value in os.environ.items() if not key.startswith('SIDEVOICE_')}
        for key in ['CURSOR_CONFIG_DIR', 'CLAUDE_CONFIG_DIR', 'CODEX_HOME']:
            env.pop(key, None)
        env.update(HOME=str(home), XDG_DATA_HOME=str(xdg), XDG_CONFIG_HOME=str(config),
                   SIDEVOICE_DATA_DIR=str(data), SIDEVOICE_CORE_PORT='0')
        bin_dir = home / '.local/bin'
        bin_dir.mkdir(mode=0o700, parents=True)
        cursor_cli = bin_dir / 'cursor'
        cursor_cli.write_text('#!/bin/sh\n[ "$1" = "--version" ] || exit 2\nprintf "cursor-fixture 1.0\\n"\n')
        cursor_cli.chmod(0o700)
        root = xdg / 'sidevoice'
        cursor = home / '.cursor/mcp.json'
        cursor_saved = None
        release_gate = base / 'manager-release'
        try:
            run(first, ['install', '--service', '--no-agents', '--json'], env)
            wait_running(first, env)
            original = selected(root)['id']

            # SIGINT while the real install lock is held cannot mutate stop intent or pointers.
            run(first, ['service', 'stop', '--json'], env)
            before = pointers(root)
            stop_file = data / 'node-stopped.json'
            stopped = stop_file.read_bytes()
            with (data / 'install.lock').open('r+') as lock:
                fcntl.flock(lock, fcntl.LOCK_EX)
                with installer(second, env, base, 'cancel') as (child, out, err):
                    wait_for(lambda: progress_seen(err, 'wait-lock'), child, 'install lock wait')
                    child.send_signal(signal.SIGINT)
                    answer = result(child, out, err)
                    assert answer.get('error', {}).get('key') == 'install.cancelled', answer
                    assert child.returncode != 0
                    assert pointers(root) == before
                    assert stop_file.read_bytes() == stopped
            run(first, ['install', '--no-agents', '--json'], env)
            wait_running(first, env)
            assert selected(root)['id'] == original

            # Existing manager override gives a deterministic gate after pointer commit,
            # before any candidate Core starts. Progress alone would race the spawn.
            wrapper, arrived, release_gate = manager_gate(base, root, original)
            fault_env = env | {('SIDEVOICE_LAUNCHCTL' if sys.platform == 'darwin'
                               else 'SIDEVOICE_SYSTEMCTL'): str(wrapper)}
            with installer(second, fault_env, base, 'verify-failure') as (child, out, err):
                wait_for(arrived.exists, child, 'committed candidate manager start', timeout=180)
                child.send_signal(signal.SIGSTOP)
                try:
                    assert progress_seen(err, 'service-start'), err.read_text()
                    candidate = selected(root)['id']
                    assert candidate != original
                    assert json.loads(arrived.read_bytes())['selected'] == candidate
                    program = root / 'releases' / candidate / 'core/bin/sidevoice-core-rust'
                    assert program.resolve().is_relative_to((root / 'releases' / candidate).resolve())
                    program.unlink()
                    program.write_text('#!/bin/sh\nexit 73\n')
                    program.chmod(0o700)
                finally:
                    release_gate.touch()
                    child.send_signal(signal.SIGCONT)
                answer = result(child, out, err)
                assert answer.get('action') == 'rollback' and answer.get('back') is True, answer
                assert answer['ok'] is False
                assert selected(root)['id'] == original
                wait_running(first, env)
                assert ready(data)['pid'] > 1
            # Failure cleanup must permit restaging the genuine embedded candidate.
            run(second, ['install', '--no-agents', '--json'], env)
            wait_running(second, env)
            upgraded = selected(root)['id']
            assert upgraded != original
            run(second, ['rollback', '--json'], env)
            wait_running(first, env)
            assert selected(root)['id'] == original

            # Invalid registration state must block success/pruning, then be repairable.
            run(first, ['agents', 'connect', 'cursor', '--json'], env)
            cursor_saved = cursor.read_bytes()
            registered = json.loads(cursor_saved)['mcpServers']['sidevoice']
            cursor.write_text('{"mcpServers":')
            cursor.chmod(0o600)
            answer, output = run(second, ['install', '--no-agents', '--json'], env, success=False)
            assert output.returncode != 0, answer
            assert answer.get('error', {}).get('key') == 'agents.reconciliation-failed', answer
            assert (root / 'releases' / original / 'release.json').is_file()
            assert Path(registered['command']).is_file(), 'failed reconciliation pruned its old command'
            cursor.write_bytes(cursor_saved)
            cursor.chmod(0o600)
            run(second, ['install', '--no-agents', '--json'], env)
            wait_running(second, env)
            assert selected(root)['id'] == upgraded
            repaired = json.loads(cursor.read_bytes())['mcpServers']['sidevoice']
            assert Path(repaired['command']).is_file()
            assert Path(repaired['command']).resolve().is_relative_to((root / 'releases' / upgraded).resolve())
            cursor_saved = cursor.read_bytes()
            print(json.dumps({'ok': True, 'checked': ['lock-cancellation-stop-preservation',
                              'automatic-verification-rollback-retry', 'reconcile-failure-retention-retry']}))
        finally:
            release_gate.touch()
            if cursor_saved is not None:
                cursor.write_bytes(cursor_saved)
                cursor.chmod(0o600)
            cleanup = subprocess.run([str(second), 'uninstall', '--json'], env=env,
                                     capture_output=True, text=True, timeout=180)
            if cleanup.returncode:
                raise AssertionError(f'failure-proof cleanup failed: {cleanup.stdout}\n{cleanup.stderr}')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--first', required=True, type=Path)
    parser.add_argument('--second', required=True, type=Path)
    args = parser.parse_args()
    main(args.first.resolve(), args.second.resolve())
