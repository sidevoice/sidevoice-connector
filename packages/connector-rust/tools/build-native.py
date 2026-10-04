"""Build one target native distributor from already verified, pinned Core inputs.

Development tooling only. The output is one Rust executable; Python and JavaScript
are never included in the installed release. Run native compilation in GitHub Actions.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[3]
TARGETS = {'Darwin:arm64': 'macos-aarch64', 'Linux:x86_64': 'linux-x86_64',
           'Linux:aarch64': 'linux-aarch64'}


def build(inputs, output):
    target = TARGETS.get(f'{os.uname().sysname}:{os.uname().machine}')
    if not target:
        raise ValueError('unsupported native target')
    inputs = inputs.resolve(strict=True)
    manifest_file = inputs / 'native-core-manifest.json'
    manifest = json.loads(manifest_file.read_bytes())
    pin = json.loads((ROOT / 'packages/connector/rust-core-production-pin.json').read_bytes())
    if manifest['source_sha'] != pin['source_sha'] or manifest['cargo_lock_sha256'] != pin['cargo_lock_sha256']:
        raise ValueError('Core inputs differ from the committed production source pin')
    record = manifest['bundles'][target]
    archive = inputs / record['name']
    if archive.parent != inputs or archive.is_symlink() or not archive.is_file():
        raise ValueError('unsafe Core archive input')
    if archive.stat().st_size != record['size'] or hashlib.sha256(archive.read_bytes()).hexdigest() != record['sha256']:
        raise ValueError('Core archive differs from its manifest')
    source = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip()
    version = json.loads((ROOT / 'packages/connector/package.json').read_bytes())['version']
    env = dict(os.environ, SIDEVOICE_CONNECTOR_BUILD_SHA=source, SIDEVOICE_CONNECTOR_TARGET=target,
               SIDEVOICE_CONNECTOR_VERSION=version, SIDEVOICE_REQUIRE_NATIVE_PAYLOAD='1',
               SIDEVOICE_NATIVE_CORE_MANIFEST=str(manifest_file), SIDEVOICE_NATIVE_CORE_ARCHIVE=str(archive))
    subprocess.run(['cargo', 'build', '--locked', '--release', '--manifest-path',
                    str(ROOT / 'packages/connector-rust/Cargo.toml')], check=True, env=env, cwd=ROOT)
    output.mkdir(parents=True, exist_ok=True)
    binary = output / 'sidevoice'
    shutil.copyfile(ROOT / 'packages/connector-rust/target/release/sidevoice-rust-proof', binary)
    binary.chmod(0o755)
    if os.uname().sysname == 'Darwin':
        subprocess.run(['codesign', '--force', '--sign', '-', str(binary)], check=True)
    identity = json.loads(subprocess.check_output([str(binary), '--version', '--json'], env=env))
    if identity.get('format') != 'rust-native' or identity.get('sea') is not False or identity.get('connector_sha') != source:
        raise ValueError('native distributor identity does not match the candidate')
    metadata = json.loads(subprocess.check_output([str(binary), 'metadata', '--json'], env=env))
    if metadata['embedded_core']['manifest_sha256'] != hashlib.sha256(manifest_file.read_bytes()).hexdigest():
        raise ValueError('native distributor contains a different Core manifest')
    print(json.dumps({'path': str(binary), 'sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
                      'size': binary.stat().st_size, 'identity': identity}, sort_keys=True))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--core-inputs', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    build(args.core_inputs, args.output)
