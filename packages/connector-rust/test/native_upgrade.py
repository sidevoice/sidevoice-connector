"""Upgrade and roll back an accepted existing Mac SEA installation using native control."""
import argparse
import json
import os
from pathlib import Path
import tempfile

from native_control import ready, run, selected, wait_running


def main(legacy, native):
    with tempfile.TemporaryDirectory(prefix='svnu-', dir='/tmp') as directory:
        home = Path(directory).resolve()
        data, xdg, config = home / '.sidevoice', home / 'data', home / 'config'
        for path in [data, xdg, config]:
            path.mkdir(mode=0o700)
        env = {key: value for key, value in os.environ.items()
               if not key.startswith('SIDEVOICE_') and key not in {'CURSOR_CONFIG_DIR', 'CLAUDE_CONFIG_DIR', 'CODEX_HOME'}}
        env.update(HOME=str(home), SIDEVOICE_DATA_DIR=str(data), XDG_DATA_HOME=str(xdg),
                   XDG_CONFIG_HOME=str(config), SIDEVOICE_CORE_PORT='0')
        root = xdg / 'sidevoice'
        try:
            run(legacy, ['install', '--service', '--no-agents', '--json'], env)
            old = selected(root)
            assert old['format'] == 'sea', old
            launch = ready(data)['launch_id']
            sentinel = data / 'operator-note.txt'
            sentinel.write_text('preserve existing unrelated state')
            run(native, ['install', '--no-agents', '--json'], env)
            wait_running(native, env)
            current = selected(root)
            assert current['format'] == 'rust-native', current
            assert current['id'] != old['id'] and ready(data)['launch_id'] != launch
            assert sentinel.read_text() == 'preserve existing unrelated state'
            run(native, ['rollback', '--json'], env)
            wait_running(native, env)
            assert selected(root)['id'] == old['id'], 'native rollback lost the existing SEA selection'
            run(native, ['install', '--no-agents', '--json'], env)
            wait_running(native, env)
            assert selected(root)['id'] == current['id']
            assert sentinel.read_text() == 'preserve existing unrelated state'
            print(json.dumps({'ok': True, 'legacy': old['id'], 'native': current['id'],
                              'checked': ['legacy-upgrade', 'legacy-rollback', 'native-reinstall', 'preserved-user-state']}))
        finally:
            run(native, ['uninstall', '--json'], env, success=False)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--legacy', required=True, type=Path)
    parser.add_argument('--native', required=True, type=Path)
    args = parser.parse_args()
    main(args.legacy.resolve(), args.native.resolve())
