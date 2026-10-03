"""Real launchd acceptance for the private Rust service profile on macOS arm64.

The fixture stages the pinned Core source and the exact release binary under a private
temporary profile, then exercises the user launchd manager. It never reads production
Sidevoice or agent configuration.
"""

import asyncio
import hashlib
import json
import os
from pathlib import Path
import plistlib
import platform
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import uuid


CORE_SHA = '4d6df599239602954a3c6ab503c642eeeab5ca12'
ENV_NAMES = (
    'HOME', 'CODEX_HOME', 'CLAUDE_CONFIG_DIR', 'CURSOR_CONFIG_DIR', 'CURSOR_DATA_DIR',
    'XDG_CONFIG_HOME', 'XDG_DATA_HOME', 'SIDEVOICE_DATA_DIR', 'SIDEVOICE_CODEX_BIN',
)


def run(argv, *, env=None, timeout=60, check=True):
    result = subprocess.run(argv, env=env, capture_output=True, text=True, timeout=timeout)
    if check and result.returncode:
        raise AssertionError(
            f'command failed ({result.returncode}): {argv!r}\n'
            f'stdout: {result.stdout[-3000:]}\nstderr: {result.stderr[-3000:]}'
        )
    return result


def mode_private(path):
    path.mkdir(parents=True, exist_ok=True, mode=0o700)
    path.chmod(0o700)


def sha256(path):
    digest = hashlib.sha256()
    with path.open('rb') as source:
        for block in iter(lambda: source.read(1 << 20), b''):
            digest.update(block)
    return digest.hexdigest()


def bootstrap_profile():
    if os.environ.get('SIDEVOICE_LAUNCHD_BOOTSTRAPPED') == '1':
        return
    if platform.system() != 'Darwin' or platform.machine() not in ('arm64', 'aarch64'):
        raise SystemExit('the real launchd fixture must run on macOS arm64')

    core_checkout = Path(os.environ['SIDEVOICE_CORE_SOURCE']).resolve()
    proof_binary = Path(os.environ['SIDEVOICE_RUST_PROOF_BIN']).resolve()
    expected_sha = os.environ.get('SIDEVOICE_CORE_SHA', CORE_SHA)
    actual_sha = run(['git', '-C', str(core_checkout), 'rev-parse', 'HEAD']).stdout.strip()
    if actual_sha != expected_sha or expected_sha != CORE_SHA:
        raise AssertionError(f'pinned Core checkout mismatch: {actual_sha} != {CORE_SHA}')
    if not proof_binary.is_file() or not os.access(proof_binary, os.X_OK):
        raise AssertionError(f'missing hosted Rust proof binary: {proof_binary}')

    runner_temp = Path(os.environ['RUNNER_TEMP']).resolve()
    profile_root = runner_temp / 'sidevoice-rust-launchd-profile'
    shutil.rmtree(profile_root, ignore_errors=True)
    profile_root.mkdir(mode=0o700)
    profile_root = profile_root.resolve()

    release_id = f'proof-{uuid.uuid4().hex[:12]}'
    release = profile_root / 'releases' / release_id
    releases = profile_root / 'releases'
    core_root = release / 'core'
    core_source = core_root / 'source'
    core_venv = core_root / 'venv'
    core_bin = core_root / 'bin'
    dist = release / 'dist'
    for path in (releases, release, core_root, core_bin, dist):
        mode_private(path)

    ignored = shutil.ignore_patterns('.git', '.venv', '__pycache__', '*.pyc', '.pytest_cache', 'build', 'dist')
    shutil.copytree(core_checkout, core_source, ignore=ignored)
    for path in (core_source, *[p for p in core_source.rglob('*') if p.is_dir()]):
        path.chmod(path.stat().st_mode & ~0o022)
    run(['uv', 'venv', '--python', '3.12', str(core_venv)], timeout=180)
    core_python = core_venv / 'bin' / 'python'
    run(['uv', 'pip', 'install', '--python', str(core_python), '-e', str(core_source), 'websockets>=15,<16'], timeout=600)

    staged_binary = dist / 'sidevoice-rust-proof'
    shutil.copy2(proof_binary, staged_binary)
    staged_binary.chmod(0o700)
    real_core_cli = core_venv / 'bin' / 'sidevoice-core'
    self_test = run([str(real_core_cli), '--self-test'], timeout=60)
    self_test_json = json.loads(self_test.stdout)
    if self_test_json.get('ok') is not True:
        raise AssertionError(f'pinned Core self-test failed: {self_test.stdout[-1000:]}')

    data = profile_root / 'sidevoice'
    home = profile_root / 'home'
    codex = profile_root / 'codex'
    claude = profile_root / 'claude'
    cursor_root = profile_root / 'cursor'
    cursor_config = cursor_root / 'config'
    cursor_data = cursor_root / 'data'
    xdg_root = profile_root / 'xdg'
    xdg_config = xdg_root / 'config'
    xdg_data = xdg_root / 'data'
    for path in (home, codex, claude, cursor_root, cursor_config, cursor_data,
                 xdg_root, xdg_config, xdg_data, data, data / 'core'):
        mode_private(path)

    (releases / 'current').symlink_to(release_id)
    hold_core = data / 'hold-core-start'
    wait_core = data / 'core-launch-waiting'
    fail_core = data / 'fail-core-start'
    failed_core = data / 'core-failed-start'
    wrapper = core_bin / 'sidevoice-core'
    wrapper.write_text(
        '#!/bin/sh\n'
        f'if [ -e {str(hold_core)!r} ]; then /usr/bin/touch {str(wait_core)!r}; '
        f'while [ -e {str(hold_core)!r} ]; do /bin/sleep 0.1; done; fi\n'
        f'if [ -e {str(fail_core)!r} ]; then /usr/bin/touch {str(failed_core)!r}; exit 1; fi\n'
        f'exec {str(real_core_cli)!r} "$@"\n'
    )
    wrapper.chmod(0o700)
    # Validate the exact launchd entrypoint while it is ungated. The self-test and digest below
    # belong to the real Core CLI in the staged, pinned runtime, not to the fixture wrapper.
    wrapper_self_test = run([str(wrapper), '--self-test'], timeout=60)
    if json.loads(wrapper_self_test.stdout).get('ok') is not True:
        raise AssertionError('the staged launchd Core entrypoint did not pass --self-test')
    hold_core.touch(mode=0o600)

    sentinel = runner_temp / 'sidevoice-real-home-sentinel'
    shutil.rmtree(sentinel, ignore_errors=True)
    for path in (sentinel, sentinel / 'codex', sentinel / 'claude', sentinel / 'cursor',
                 sentinel / 'xdg-config', sentinel / 'xdg-data', sentinel / 'sidevoice'):
        mode_private(path)
    sentinel_files = {
        sentinel / 'codex' / 'config.toml': b'[mcp_servers.sidevoice]\ncommand = "/foreign/codex"\nargs = ["mcp"]\n',
        sentinel / 'claude' / 'settings.json': b'{"sentinel":"claude"}\n',
        sentinel / 'cursor' / 'mcp.json': b'{"mcpServers":{"sentinel":{"command":"foreign"}}}\n',
        sentinel / 'sidevoice' / 'sentinel.json': b'{"sentinel":"sidevoice"}\n',
    }
    for path, contents in sentinel_files.items():
        path.write_bytes(contents)
        path.chmod(0o600)
    (sentinel / 'xdg-config' / 'sentinel.json').write_text('{"sentinel":"xdg-config"}\n')
    (sentinel / 'xdg-data' / 'sentinel.json').write_text('{"sentinel":"xdg-data"}\n')
    sentinel_snapshot = {str(path): sha256(path) for path in sentinel.rglob('*') if path.is_file()}

    fake_codex = home / '.local' / 'bin' / 'codex'
    mode_private(fake_codex.parent)
    codex_entry = {
        'name': 'sidevoice',
        'transport': {'type': 'stdio', 'command': str(staged_binary),
                      'args': ['mcp', '--profile-root', str(profile_root)]},
        'enabled': True,
    }
    codex_json = codex / 'fake-sidevoice.json'
    codex_json.write_text(json.dumps(codex_entry))
    codex_json.chmod(0o600)
    config = codex / 'config.toml'
    config.write_text(
        '[mcp_servers.sidevoice]\n'
        f'command = {json.dumps(str(staged_binary))}\n'
        f'args = {json.dumps(codex_entry["transport"]["args"])}\n'
    )
    config.chmod(0o600)
    env_capture = data / 'codex-environment.txt'
    slow_next_get = data / 'slow-next-get'
    slow_get_started = data / 'slow-get-started'
    queue_receipt = data / 'queue.receipt'
    fake_codex.write_text(
        '#!/bin/sh\n'
        f'if [ "$1" = "--version" ]; then /usr/bin/printf "private Codex fixture 1\\n"; exit 0; fi\n'
        'if [ "$1" = "mcp" ] && [ "$2" = "get" ] && [ "$3" = "sidevoice" ]; then\n'
        f'  /usr/bin/printf "%s\\n" "$HOME" "$CODEX_HOME" "$CLAUDE_CONFIG_DIR" "$CURSOR_CONFIG_DIR" "$CURSOR_DATA_DIR" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$SIDEVOICE_DATA_DIR" > {str(env_capture)!r}\n'
        f'  if [ -e {str(slow_next_get)!r} ]; then /bin/rm -f {str(slow_next_get)!r}; /usr/bin/touch {str(slow_get_started)!r}; /bin/sleep 2; fi\n'
        f'  /bin/cat {str(codex_json)!r}; exit 0\n'
        'fi\n'
        'if [ "$1" = "queue" ] && [ "$2" = "--thread" ] && [ "$4" = "--message" ]; then\n'
        f'  /usr/bin/printf "%s\\n---\\n" "$5" >> {str(queue_receipt)!r}; exit 0\n'
        'fi\n'
        'exit 2\n'
    )
    fake_codex.chmod(0o700)

    manager_environment = {
        'HOME': str(sentinel),
        'CODEX_HOME': str(sentinel / 'codex'),
        'CLAUDE_CONFIG_DIR': str(sentinel / 'claude'),
        'CURSOR_CONFIG_DIR': str(sentinel / 'cursor'),
        'CURSOR_DATA_DIR': str(sentinel / 'cursor' / 'data'),
        'XDG_CONFIG_HOME': str(sentinel / 'xdg-config'),
        'XDG_DATA_HOME': str(sentinel / 'xdg-data'),
        'SIDEVOICE_DATA_DIR': str(sentinel / 'sidevoice'),
        # A private test CLI lets real Core input delivery be observed without invoking a user's Codex.
        'SIDEVOICE_CODEX_BIN': str(fake_codex),
    }
    env = os.environ.copy()
    env.update(manager_environment)
    env.update({
        'SIDEVOICE_LAUNCHD_BOOTSTRAPPED': '1',
        'SIDEVOICE_PROFILE_ROOT': str(profile_root),
        'SIDEVOICE_REAL_HOME_SENTINEL': str(sentinel),
        'SIDEVOICE_SENTINEL_SNAPSHOT': json.dumps(sentinel_snapshot),
        'SIDEVOICE_CORE_CLI': str(real_core_cli),
        'SIDEVOICE_CORE_SHA': actual_sha,
        'SIDEVOICE_CORE_CLI_SHA256': sha256(real_core_cli),
        'SIDEVOICE_RUST_BINARY_SHA256': sha256(staged_binary),
        'SIDEVOICE_SELF_TEST': json.dumps(self_test_json, sort_keys=True),
    })
    os.execve(str(core_python), [str(core_python), str(Path(__file__).resolve())], env)


def launchctl_env(name):
    result = run(['/bin/launchctl', 'getenv', name], check=False)
    return result.stdout.rstrip('\n') if result.returncode == 0 else None


def set_launchctl_env(name, value):
    if value is None:
        run(['/bin/launchctl', 'unsetenv', name], check=False)
    else:
        run(['/bin/launchctl', 'setenv', name, value])


def write_evidence(profile, values):
    path = profile / 'service-evidence.json'
    path.write_text(json.dumps(values, indent=2, sort_keys=True) + '\n')
    path.chmod(0o600)


async def exercise():
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    from interop_core import finish, http_json, http_response, mcp_request, process_alive, tool, until
    from websockets.asyncio.client import unix_connect

    profile = Path(os.environ['SIDEVOICE_PROFILE_ROOT']).resolve()
    release = profile / 'releases' / os.readlink(profile / 'releases/current')
    staged_binary = release / 'dist' / 'sidevoice-rust-proof'
    real_core_cli = Path(os.environ['SIDEVOICE_CORE_CLI'])
    data = profile / 'sidevoice'
    core_data = data / 'core'
    socket_path = core_data / 'local.sock'
    connector_socket = data / 'connector.sock'
    services = data / 'service'
    codex = profile / 'codex'
    slow_next_get = data / 'slow-next-get'
    slow_get_started = data / 'slow-get-started'
    queue_receipt = data / 'queue.receipt'
    hold_core = data / 'hold-core-start'
    wait_core = data / 'core-launch-waiting'
    fail_core = data / 'fail-core-start'
    failed_core = data / 'core-failed-start'
    sentinel = Path(os.environ['SIDEVOICE_REAL_HOME_SENTINEL'])
    saved_sentinel = json.loads(os.environ['SIDEVOICE_SENTINEL_SNAPSHOT'])
    uid = os.geteuid()
    domain = f'gui/{uid}'
    env_restore = {name: launchctl_env(name) for name in ENV_NAMES}
    installed = False
    daemon_log = None
    facade = None
    command_tasks = []
    scan_tasks = []
    labels = []
    result = {'core_sha': os.environ['SIDEVOICE_CORE_SHA'],
              'core_cli_sha256': os.environ['SIDEVOICE_CORE_CLI_SHA256'],
              'rust_binary_sha256': os.environ['SIDEVOICE_RUST_BINARY_SHA256'],
              'core_self_test': json.loads(os.environ['SIDEVOICE_SELF_TEST'])}

    expected_core_env = {
        'HOME': str(profile / 'home'),
        'XDG_CONFIG_HOME': str(profile / 'xdg/config'),
        'XDG_DATA_HOME': str(profile / 'xdg/data'),
        'SIDEVOICE_DATA_DIR': str(data),
    }
    expected_connector_env = {
        'HOME': str(profile / 'home'),
        'CLAUDE_CONFIG_DIR': str(profile / 'claude'),
        'CODEX_HOME': str(codex),
        'CURSOR_CONFIG_DIR': str(profile / 'cursor/config'),
        'CURSOR_DATA_DIR': str(profile / 'cursor/data'),
        'XDG_CONFIG_HOME': str(profile / 'xdg/config'),
        'XDG_DATA_HOME': str(profile / 'xdg/data'),
        'SIDEVOICE_DATA_DIR': str(data),
        'SIDEVOICE_SERVICE': 'launchd',
    }
    control_env = os.environ.copy()
    control_env.update({
        'HOME': str(sentinel),
        'CODEX_HOME': str(sentinel / 'codex'),
        'CLAUDE_CONFIG_DIR': str(sentinel / 'claude'),
        'CURSOR_CONFIG_DIR': str(sentinel / 'cursor'),
        'CURSOR_DATA_DIR': str(sentinel / 'cursor/data'),
        'XDG_CONFIG_HOME': str(sentinel / 'xdg-config'),
        'XDG_DATA_HOME': str(sentinel / 'xdg-data'),
        'SIDEVOICE_DATA_DIR': str(sentinel / 'sidevoice'),
    })

    async def service(action, *, expect_ok=True, timeout_seconds=90):
        process = await asyncio.create_subprocess_exec(
            str(staged_binary), '--profile-root', str(profile), 'service', action, '--json',
            stdin=asyncio.subprocess.DEVNULL, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
            env=control_env,
        )
        try:
            stdout, stderr = await asyncio.wait_for(process.communicate(), timeout_seconds)
        except BaseException:
            if process.returncode is None:
                process.kill()
                await process.wait()
            raise
        text = stdout.decode(errors='replace')
        if len(text.splitlines()) != 1:
            raise AssertionError(f'service {action} must print exactly one JSON line: {text[-1000:]!r}; {stderr[-1000:]!r}')
        value = json.loads(text)
        if expect_ok and (process.returncode != 0 or value.get('ok') is not True):
            manager = []
            for label in labels:
                snapshot = run(['/bin/launchctl', 'print', f'{domain}/{label}'], check=False)
                manager.append(f'{label}: rc={snapshot.returncode} '
                               f'{snapshot.stdout[-4000:]} {snapshot.stderr[-1000:]}')
            status = run_status_sync(staged_binary, profile, control_env)
            raise AssertionError(
                f'service {action} failed: {value}; {stderr.decode(errors="replace")[-2000:]}\n'
                f'current service status: {status}\nlaunchd snapshots:\n' + '\n'.join(manager)
            )
        if not expect_ok and (process.returncode == 0 and value.get('ok') is True):
            raise AssertionError(f'service {action} unexpectedly succeeded: {value}')
        return value

    async def wait_file(path, description, seconds=20):
        return await until(path.exists, description, seconds=seconds)

    async def wait_core_host_link():
        deadline = time.monotonic() + 25
        last_error = None
        while time.monotonic() < deadline:
            try:
                return await asyncio.to_thread(http_json, core_data / 'local.sock', 'GET',
                    '/api/host/agents?rescan=1&watch=codex', None, token)
            except Exception as error:
                last_error = error
                await asyncio.sleep(.2)
        raise AssertionError(f'Core host API did not reconnect to Rust after restart: {last_error!r}') from last_error

    async def connector_pid(label):
        def read_pid():
            value = run(['/bin/launchctl', 'print', f'{domain}/{label}'], check=False)
            if value.returncode:
                return None
            match = re.search(r'^\s*pid = (\d+)\s*$', value.stdout, re.M)
            return int(match.group(1)) if match else None
        return await until(read_pid, f'launchd PID for {label}', seconds=10)

    async def ipc(method, params=None):
        reader, writer = await asyncio.open_unix_connection(str(connector_socket))
        try:
            writer.write((json.dumps({'id': 99, 'method': method, 'params': params or {}}) + '\n').encode())
            await writer.drain()
            response = json.loads(await asyncio.wait_for(reader.readline(), 5))
            assert response.get('ok') is True, response
            return response['result']
        finally:
            writer.close()
            await writer.wait_closed()

    async def wait_identity(predicate, description, seconds=15):
        deadline = time.monotonic() + seconds
        last = None
        while time.monotonic() < deadline:
            try:
                last = await ipc('identity')
                if predicate(last):
                    return last
            except (FileNotFoundError, ConnectionRefusedError, asyncio.TimeoutError, OSError):
                pass
            await asyncio.sleep(.1)
        raise AssertionError(f'timed out waiting for {description}; last identity: {last!r}')

    async def start_on_demand():
        process = await asyncio.create_subprocess_exec(
            str(staged_binary), '--profile-root', str(profile), 'connector',
            stdin=asyncio.subprocess.DEVNULL, stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.DEVNULL, env={**control_env, **expected_connector_env},
        )
        identity = await wait_identity(lambda value: value.get('managed') is False,
                                       'private on-demand connector identity')
        assert identity['pid'] == process.pid, (identity, process.pid)
        return process, identity

    async def stop_process(process, description):
        await asyncio.wait_for(process.wait(), 25)
        assert not process_alive(process.pid), f'{description} PID still exists: {process.pid}'
        assert not connector_socket.exists(), f'{description} left connector socket behind'

    async def start_facade(thread):
        process = await asyncio.create_subprocess_exec(
            str(staged_binary), '--profile-root', str(profile), 'mcp',
            stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=daemon_log,
            env={**control_env, 'CODEX_THREAD_ID': thread},
        )
        await mcp_request(process, 'initialize', {'protocolVersion': '2025-06-18', 'capabilities': {},
                         'clientInfo': {'name': 'codex', 'version': 'launchd-proof'}}, 1)
        process.stdin.write(b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
        await process.stdin.drain()
        return process

    try:
        for name, value in manager_environment_from_profile(profile, sentinel).items():
            set_launchctl_env(name, value)

        # Refusing an unsafe second definition must leave both stop intent and the first
        # definition untouched. This exercises install's preflight while install.lock is held.
        suffix = hashlib.sha256(os.fsencode(str(profile))).hexdigest()[:16]
        services.mkdir(mode=0o700)
        core_definition = services / f'dev.sidevoice.rustproof.{suffix}.core.plist'
        connector_definition = services / f'dev.sidevoice.rustproof.{suffix}.connector.plist'
        unsafe_target = data / 'unsafe-connector-definition.plist'
        unsafe_target.write_bytes(b'foreign target must not be opened\n')
        unsafe_target.chmod(0o600)
        core_definition.write_bytes(b'original private Core definition\n')
        core_definition.chmod(0o600)
        connector_definition.symlink_to(unsafe_target)
        stop_marker = data / 'node-stopped.json'
        stop_marker.write_bytes(b'{"stopped":true,"sentinel":"preserve-on-refusal"}\n')
        stop_marker.chmod(0o600)
        original_core_definition = core_definition.read_bytes()
        original_stop_marker = stop_marker.read_bytes()
        refusal = await service('install', expect_ok=False)
        assert refusal.get('error', {}).get('key') == 'service.definition-unsafe', refusal
        assert stop_marker.read_bytes() == original_stop_marker
        assert core_definition.read_bytes() == original_core_definition
        assert connector_definition.is_symlink() and connector_definition.resolve() == unsafe_target
        connector_definition.unlink()
        core_definition.unlink()
        stop_marker.unlink()
        result['unsafe_connector_preflight_preserves_stop_and_core_definition'] = True

        # Open an on-demand façade and create a real pending binding before install. Installing
        # the two launchd jobs must shut down that verified daemon, then hand socket ownership
        # to the running launchd Connector while Core is still behind its gate.
        thread = str(uuid.uuid4())
        daemon_log = (data / 'service' / 'facade.log').open('ab')
        facade = await start_facade(thread)
        on_demand = await wait_identity(lambda value: value.get('managed') is False,
                                       'initial on-demand connector')
        assert process_alive(on_demand['pid']), on_demand
        local_join = await tool(facade, 'voice_connect', {'title': 'Private launchd service proof'}, 2, thread)
        assert local_join['conversation'] == thread and local_join['binding_id'].startswith('local-'), local_join
        result['on_demand_facade_binding_before_install'] = True

        # Check the full environment and exact argv serialized by the immutable private spec.
        install_task = asyncio.create_task(service('install', timeout_seconds=90))
        command_tasks.append(install_task)
        await wait_file(wait_core, 'Core job reached its deliberate pre-start gate', seconds=30)
        await until(connector_socket.exists, 'managed connector socket before Core is ready', seconds=30)
        connector_label = f'dev.sidevoice.rustproof.{suffix}.connector'
        managed_pid = await connector_pid(connector_label)
        managed_identity = await wait_identity(
            lambda value: value.get('managed') is True and value.get('pid') == managed_pid,
            'launchd-owned connector socket after on-demand handoff', seconds=20,
        )
        assert managed_identity['pid'] != on_demand['pid'], (managed_identity, on_demand)
        await until(lambda: not process_alive(on_demand['pid']), 'old on-demand daemon exit', seconds=10)
        result['install_handed_socket_to_launchd_connector'] = {
            'old_pid': on_demand['pid'], 'managed_pid': managed_identity['pid'],
        }
        assert not (core_data / 'core.json').exists(), 'Core became ready before the test released its gate'
        early = await service('status')
        assert early['state'] == 'starting' and early['connector']['running'] is True, early
        result['connector_socket_before_core_ready'] = True
        hold_core.unlink()
        await wait_file(core_data / 'core.json', 'pinned Core ready after late startup', seconds=60)
        installed_status = await install_task
        assert installed_status['state'] == 'running' and installed_status['reachable'] is True, installed_status
        installed = True
        result['install_state'] = installed_status['state']

        plist_paths = sorted(services.glob('*.plist'))
        assert len(plist_paths) == 2, plist_paths
        plists = [plistlib.loads(path.read_bytes()) for path in plist_paths]
        core_plist = next(value for value in plists if value['Label'].endswith('.core'))
        connector_plist = next(value for value in plists if value['Label'].endswith('.connector'))
        labels = [core_plist['Label'], connector_plist['Label']]
        assert all(label.startswith('dev.sidevoice.rustproof.') for label in labels), labels
        assert not any(label in ('dev.sidevoice.core', 'dev.sidevoice.connector') for label in labels)
        assert core_plist['EnvironmentVariables'] == expected_core_env, core_plist['EnvironmentVariables']
        assert connector_plist['EnvironmentVariables'] == expected_connector_env, connector_plist['EnvironmentVariables']
        assert core_plist['ProgramArguments'] == [
            str(profile / 'releases/current/core/bin/sidevoice-core'), '--data-dir', str(core_data),
            '--socket', str(socket_path), '--room-credential', str(data / 'credentials.json'),
            '--port', '0', '--idle-exit', '0',
        ], core_plist['ProgramArguments']
        assert connector_plist['ProgramArguments'] == [
            str(profile / 'releases/current/dist/sidevoice-rust-proof'), 'connector', '--service',
            '--profile-root', str(profile),
        ], connector_plist['ProgramArguments']
        assert core_plist['RunAtLoad'] is True
        assert core_plist['KeepAlive'] == {'SuccessfulExit': False, 'Crashed': True}
        assert connector_plist['RunAtLoad'] is True and connector_plist['KeepAlive'] is True
        for path in plist_paths:
            run(['/usr/bin/plutil', '-lint', str(path)])
        result['launchd_labels'] = labels
        result['plist_private_environment_exact'] = True

        connector_process = await connector_pid(connector_plist['Label'])
        core_before = installed_status['core']['pid']
        assert process_alive(connector_process) and process_alive(core_before)
        # The managed connector must not use the one-shot 15 second idle exit with no façade.
        await asyncio.sleep(16)
        after_idle = await service('status')
        assert after_idle['state'] == 'running' and after_idle['connector']['running'] is True, after_idle
        assert process_alive(connector_process), 'managed connector exited while no conversation was open'
        result['zero_facade_after_16s'] = True

        # A real ready Core that cannot answer health is a hang, not merely a missing-ready startup.
        os.kill(core_before, signal.SIGSTOP)
        try:
            hung = await service('status')
            assert hung['state'] == 'failed' and hung.get('failure', {}).get('key') == 'hang', hung
            assert hung['reachable'] is False, hung
        finally:
            os.kill(core_before, signal.SIGCONT)
        await until(lambda: run_status_sync(staged_binary, profile, control_env).get('state') == 'running',
                    'Core health after SIGCONT', seconds=15)
        result['ready_but_hung_core_is_reported_as_hang'] = True

        captured_env = await until(lambda: env_capture_path(data).exists(), 'private Codex scan environment')
        captured = env_capture_path(data).read_text().splitlines()
        assert captured == [expected_connector_env[name] for name in (
            'HOME', 'CODEX_HOME', 'CLAUDE_CONFIG_DIR', 'CURSOR_CONFIG_DIR', 'CURSOR_DATA_DIR',
            'XDG_CONFIG_HOME', 'XDG_DATA_HOME', 'SIDEVOICE_DATA_DIR')], captured
        result['agent_child_environment_private'] = True

        # A façade and local Core input exercise the manager-owned connector and its actual v3 link.
        joined = None
        binding_deadline = time.monotonic() + 20
        while time.monotonic() < binding_deadline:
            try:
                joined = await tool(facade, 'voice_connect', {'title': 'Private launchd service proof'}, 3, thread)
                if not joined['binding_id'].startswith('local-'):
                    break
            except Exception:
                await asyncio.sleep(.2)
        assert joined is not None, 'MCP façade did not reconnect after service install'
        assert joined['conversation'] == thread and not joined['binding_id'].startswith('local-'), joined
        token = http_json(core_data / 'local.sock', 'POST', '/api/device/local/pair', {'name': 'launchd-proof'})['token']
        history = lambda: http_json(core_data / 'local.sock', 'GET',
                                   f'/api/presentation/history?thread_id={thread}', token=token)['messages']
        async with unix_connect(path=str(core_data / 'local.sock'), uri='ws://localhost/api/presentation/ws',
                                subprotocols=['sidevoice', 'sidevoice.token.' + token]) as websocket:
            await websocket.send(json.dumps({'label': 'rtvi-ai', 'type': 'client-ready', 'id': 'ready',
                                             'data': {'settings': {'turn_end_mode': 'timer'}}}))
            session = None
            while session is None:
                frame = json.loads(await asyncio.wait_for(websocket.recv(), 20))
                if frame.get('type') == 'voice-session':
                    session = frame['data']['session_id']
            selected = http_json(core_data / 'local.sock', 'POST', '/api/presentation/select',
                                 {'session_id': session, 'thread_id': thread}, token)
            slow_next_get.touch(mode=0o600)
            scan_task = asyncio.create_task(asyncio.to_thread(
                http_json, core_data / 'local.sock', 'GET', '/api/host/agents?rescan=1&watch=codex', None, token))
            scan_tasks.append(scan_task)
            await wait_file(slow_get_started, 'bounded Rust host-agent scan started', seconds=8)
            text = 'Core input accepted by the launchd Rust connector during HostAgents scan'
            sent = await asyncio.to_thread(http_json, core_data / 'local.sock', 'POST', '/api/presentation/text', {
                'text': text, 'session_id': session, 'thread_id': thread,
                'binding_id': selected['binding']['binding_id'], 'message_id': str(uuid.uuid4()),
            }, token)
            assert sent.get('accepted') is True, sent
            await until(lambda: queue_receipt.exists() and text in queue_receipt.read_text(),
                        'Codex delivery receipt while scan is pending', seconds=3)
            delivered = await until(lambda: next((row for row in history()
                if row['id'] == sent['id'] and row['status'] == 'delivered'), None),
                'Core delivered receipt while HostAgents scan is pending', seconds=3)
            assert delivered['status'] == 'delivered'
            assert not scan_task.done(), 'the HostAgents scan completed before Core delivery acknowledgement'
            scan_result = await asyncio.wait_for(scan_task, 10)
            assert any(agent['id'] == 'codex' for agent in scan_result['agents']), scan_result
            result['core_delivery_during_scan'] = {'input': sent['id'], 'receipt': delivered['status']}

        node_status = await ipc('node.status')
        public_status = await service('status')
        assert node_status == public_status, {'node.status': node_status, 'service.status': public_status}
        result['node_status_matches_service_status'] = True

        # Core's own launchd job restarts after a real SIGKILL; the connector job remains the same process.
        old_launch = public_status['core']['launch_id']
        os.kill(public_status['core']['pid'], signal.SIGKILL)
        await until(lambda: (lambda state: state.get('state') == 'running'
            and state.get('core', {}).get('launch_id') not in (None, old_launch))(run_status_sync(staged_binary, profile, control_env)),
            'launchd Core restart and Rust reconnect', seconds=80)
        restarted_status = await service('status')
        assert restarted_status['state'] == 'running' and restarted_status['core']['launch_id'] != old_launch
        assert await connector_pid(connector_plist['Label']) == connector_process
        result['core_sigkill_restart'] = True

        # A failing Core entrypoint must not be reported healthy. A bounded explicit Core retry succeeds once
        # the fixture removes its failure marker; restart leaves the connector process alone.
        fail_core.touch(mode=0o600)
        os.kill(restarted_status['core']['pid'], signal.SIGKILL)
        await wait_file(failed_core, 'injected failing Core start', seconds=20)
        failed_snapshot = await until(
            lambda: (lambda state: state if state.get('state') != 'running' else None)(
                run_status_sync(staged_binary, profile, control_env)),
            'failed Core is observed as not running', seconds=6,
        )
        fail_core.unlink()
        recovered = await service('restart', timeout_seconds=30)
        assert recovered['state'] == 'running', recovered
        assert await connector_pid(connector_plist['Label']) == connector_process
        result['failed_core_then_explicit_retry'] = True

        # Start is the sole explicit transition that clears stop intent. A direct manager retry sees the
        # durable marker and cannot bind/reinitialize the managed connector.
        await wait_core_host_link()
        result['core_host_link_reconnected_after_explicit_retry'] = True
        slow_next_get.touch(mode=0o600)
        slow_get_started.unlink(missing_ok=True)
        stop_scan = asyncio.create_task(asyncio.to_thread(
            http_json, core_data / 'local.sock', 'GET', '/api/host/agents?rescan=1&watch=codex', None, token))
        scan_tasks.append(stop_scan)
        try:
            await wait_file(slow_get_started, 'HostAgents scan before service stop', seconds=8)
        except Exception:
            if stop_scan.done():
                try:
                    print(f'HostAgents scan before stop result: {stop_scan.result()!r}', file=sys.stderr)
                except Exception as error:
                    print(f'HostAgents scan before stop failed: {error!r}', file=sys.stderr)
            raise
        stop_task = asyncio.create_task(service('stop', timeout_seconds=60))
        command_tasks.append(stop_task)
        await wait_file(data / 'node-stopped.json', 'durable stop-intent marker', seconds=10)
        marker_before_retry = (data / 'node-stopped.json').read_bytes()
        direct = await asyncio.create_subprocess_exec(
            str(staged_binary), 'connector', '--service', '--profile-root', str(profile),
            stdin=asyncio.subprocess.DEVNULL, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
            env={**control_env, **expected_connector_env},
        )
        direct_out, direct_err = await asyncio.wait_for(direct.communicate(), 10)
        assert direct.returncode == 0, (direct_out, direct_err)
        assert (data / 'node-stopped.json').read_bytes() == marker_before_retry
        stopped = await stop_task
        assert stopped['state'] == 'stopped-by-person', stopped
        if not stop_scan.done():
            await asyncio.gather(stop_scan, return_exceptions=True)
        assert not connector_socket.exists(), 'successful stop returned while the connector socket remained'
        assert not process_alive(connector_process) and not process_alive(restarted_status['core']['pid'])
        before = (data / 'agents.json').read_bytes() if (data / 'agents.json').exists() else None
        await asyncio.sleep(0.6)
        after = (data / 'agents.json').read_bytes() if (data / 'agents.json').exists() else None
        assert before == after, 'a HostAgents operation committed after the managed daemon stopped'
        stopped_mcp = await asyncio.create_subprocess_exec(
            str(staged_binary), '--profile-root', str(profile), 'mcp',
            stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
            env=control_env,
        )
        mcp_out, mcp_err = await asyncio.wait_for(stopped_mcp.communicate(), 15)
        assert stopped_mcp.returncode != 0 and not mcp_out and b'stopped' in mcp_err.lower(), (mcp_out, mcp_err)
        assert (data / 'node-stopped.json').exists()
        result['stop_marker_retry_and_no_late_scan_commit'] = True

        # Once launchd has stopped, the private unmanaged process is still stoppable through
        # the service command. This verifies the second half of the same bounded handoff path.
        stop_marker.unlink()
        on_demand_stop, on_demand_stop_identity = await start_on_demand()
        stopped_on_demand = await service('stop')
        assert stopped_on_demand['state'] == 'stopped-by-person', stopped_on_demand
        await stop_process(on_demand_stop, 'service stop of on-demand Connector')
        assert (data / 'node-stopped.json').exists()
        result['stop_shuts_down_verified_on_demand_connector'] = on_demand_stop_identity['pid']

        started = await service('start')
        assert started['state'] == 'running' and not (data / 'node-stopped.json').exists(), started
        connector_after_start = await connector_pid(connector_plist['Label'])
        assert process_alive(connector_after_start), connector_after_start
        result['explicit_start_cleared_marker'] = True

        # Core-only restart maintains the same connector PID. Existing install is idempotent and does not
        # rewrite byte-identical launchd definitions.
        before_definitions = {path.name: path.read_bytes() for path in services.glob('*.plist')}
        core_id = started['core']['launch_id']
        restarted = await service('restart')
        assert restarted['state'] == 'running' and restarted['core']['launch_id'] != core_id
        assert await connector_pid(connector_plist['Label']) == connector_after_start
        await service('install')
        assert before_definitions == {path.name: path.read_bytes() for path in services.glob('*.plist')}
        result['core_only_restart_and_idempotent_install'] = True

        # Teardown is based on the Profile and owned definitions, not on a live current target.
        await service('stop')
        (profile / 'releases/current').unlink()
        missing_current = await service('status')
        assert missing_current['state'] == 'stopped-by-person' and missing_current['installed'] is False
        stop_marker.unlink()
        on_demand_uninstall, on_demand_uninstall_identity = await start_on_demand()
        uninstalled = await service('uninstall')
        assert not list(services.glob('*.plist')) and not connector_socket.exists()
        await stop_process(on_demand_uninstall, 'uninstall of on-demand Connector')
        assert (data / 'node-stopped.json').exists()
        second_uninstall = await service('uninstall')
        absent = await service('status')
        assert absent['state'] == 'absent' and second_uninstall['ok'] is True
        for label in labels:
            result_manager = run(['/bin/launchctl', 'print', f'{domain}/{label}'], check=False)
            assert result_manager.returncode != 0, f'launchd job survived uninstall: {label}'
        assert not any(process_alive(pid) for pid in
                       (connector_process, connector_after_start, core_before, restarted_status['core']['pid']))
        result['uninstall_missing_current_and_idempotent'] = {
            'on_demand_pid': on_demand_uninstall_identity['pid'],
        }

        actual_sentinel = {str(path): sha256(path) for path in sentinel.rglob('*') if path.is_file()}
        assert actual_sentinel == saved_sentinel, {'before': saved_sentinel, 'after': actual_sentinel}
        result['foreign_home_sentinels_unchanged'] = True
        result['final_state'] = absent['state']

        # With the façade closed and no service jobs installed, frequent status probes must
        # leave the unmanaged daemon's 15 second idle exit untouched.
        if facade:
            await finish(facade)
            facade = None
        (profile / 'releases/current').symlink_to(release.name)
        await service('start')
        idle_facade = await start_facade(str(uuid.uuid4()))
        await finish(idle_facade)
        idle_identity = await wait_identity(lambda value: value.get('managed') is False,
                                            'idle on-demand daemon for status polling')
        poll_deadline = time.monotonic() + 22
        poll_starts = []
        while time.monotonic() < poll_deadline and (
            connector_socket.exists() or process_alive(idle_identity['pid'])
        ):
            poll_starts.append(time.monotonic())
            await asyncio.to_thread(run_status_sync, staged_binary, profile, control_env)
            await asyncio.sleep(.08)
        poll_gaps = [after - before for before, after in zip(poll_starts, poll_starts[1:])]
        polls = len(poll_starts)
        max_poll_gap = max(poll_gaps, default=0)
        assert polls >= 80, f'expected frequent status probes across idle window, got {polls}'
        assert max_poll_gap < .2, (
            f'status probes were not spaced below the idle-check interval: {max_poll_gap:.3f}s'
        )
        assert not connector_socket.exists() and not process_alive(idle_identity['pid']), {
            'polls': polls, 'identity': idle_identity,
            'max_poll_gap_ms': round(max_poll_gap * 1000),
            'status': await asyncio.to_thread(run_status_sync, staged_binary, profile, control_env),
        }
        result['status_probes_do_not_extend_on_demand_idle_deadline'] = {
            'polls': polls, 'pid': idle_identity['pid'],
            'max_poll_gap_ms': round(max_poll_gap * 1000),
        }
        write_evidence(profile, result)
        print(json.dumps(result, sort_keys=True))
    finally:
        if scan_tasks:
            await asyncio.gather(*scan_tasks, return_exceptions=True)
        for task in command_tasks:
            if not task.done():
                task.cancel()
        if command_tasks:
            await asyncio.gather(*command_tasks, return_exceptions=True)
        if facade:
            await finish(facade)
        if daemon_log:
            daemon_log.close()
        if installed or list(services.glob('*.plist')):
            try:
                await service('uninstall', timeout_seconds=45)
            except Exception as error:
                print(f'cleanup service uninstall failed: {error!r}', file=sys.stderr)
            for label in labels:
                run(['/bin/launchctl', 'bootout', f'{domain}/{label}'], check=False)
        for name, value in env_restore.items():
            set_launchctl_env(name, value)
        # Retain the private profile and logs in runner temp for the always-upload diagnostic artifact.


def manager_environment_from_profile(profile, sentinel):
    return {
        'HOME': str(sentinel),
        'CODEX_HOME': str(sentinel / 'codex'),
        'CLAUDE_CONFIG_DIR': str(sentinel / 'claude'),
        'CURSOR_CONFIG_DIR': str(sentinel / 'cursor'),
        'CURSOR_DATA_DIR': str(sentinel / 'cursor' / 'data'),
        'XDG_CONFIG_HOME': str(sentinel / 'xdg-config'),
        'XDG_DATA_HOME': str(sentinel / 'xdg-data'),
        'SIDEVOICE_DATA_DIR': str(sentinel / 'sidevoice'),
        'SIDEVOICE_CODEX_BIN': str(profile / 'home' / '.local' / 'bin' / 'codex'),
    }


def run_status_sync(binary, profile, env):
    result = run([str(binary), '--profile-root', str(profile), 'service', 'status', '--json'],
                 env=env, timeout=15)
    lines = result.stdout.splitlines()
    if len(lines) != 1:
        return {}
    return json.loads(lines[0])


def env_capture_path(data):
    return data / 'codex-environment.txt'


def main():
    bootstrap_profile()
    try:
        asyncio.run(exercise())
    except Exception:
        profile = Path(os.environ.get('SIDEVOICE_PROFILE_ROOT', '/nonexistent'))
        for path in (profile / 'sidevoice/service/core.log', profile / 'sidevoice/service/connector.log',
                     profile / 'sidevoice/core/core-app.log',
                     profile / 'sidevoice/service/facade.log', profile / 'service-evidence.json'):
            if path.exists():
                print(f'{path.name}: {path.read_text(errors="replace")[-12000:]}', file=sys.stderr)
        raise


if __name__ == '__main__':
    main()
