"""Fail closed if obsolete JavaScript publication or runtime enters the native handoff."""
import argparse
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[3]


def check_source():
    package = json.loads((ROOT / 'packages/connector/package.json').read_bytes())
    if package.get('private') is not True or package.get('bin') or package.get('files'):
        raise ValueError('legacy npm package still exposes a shipped runtime')
    if package['scripts'].get('prepack') != 'node refuse-legacy-pack.mjs':
        raise ValueError('legacy runtime packaging guard is absent')


def check_artifact(directory):
    names = sorted(path.name for path in directory.iterdir())
    if names != ['sidevoice']:
        raise ValueError(f'native handoff must contain only sidevoice, got {names}')
    binary = directory / 'sidevoice'
    if binary.is_symlink() or not binary.is_file():
        raise ValueError('native executable must be a regular file')
    with binary.open('rb') as source:
        magic = source.read(4)
    if magic not in (b'\x7fELF', b'\xcf\xfa\xed\xfe', b'\xfe\xed\xfa\xcf'):
        raise ValueError('handoff is not a native ELF/Mach-O executable')
    identity = json.loads(subprocess.check_output([str(binary.resolve()), '--version', '--json']))
    if identity.get('format') != 'rust-native' or identity.get('sea') is not False:
        raise ValueError('handoff still contains the obsolete JavaScript distributor')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source-only', action='store_true')
    parser.add_argument('--artifact', type=Path)
    args = parser.parse_args()
    check_source()
    if args.artifact:
        check_artifact(args.artifact)
    elif not args.source_only:
        parser.error('--artifact or --source-only is required')
