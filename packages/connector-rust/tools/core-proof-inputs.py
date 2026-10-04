"""Reuse exact accepted Core bytes for migration tests, without rebuilding Core.

This is a temporary, expiring hosted-proof input, not a production distribution source.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import zipfile

ARTIFACT = 11299229519
RUN = 37193213949
PRODUCER = '435fd315e657a4f1372fc524f773387797195f38'
ZIP_SHA256 = 'aed0481901df866ff445dee67aa80fb03c5182189f6e26789ec49153977676e9'
REPOSITORY = 'sidevoice/sidevoice-connector'


def fetch(output):
    metadata = json.loads(subprocess.check_output(['gh', 'api', f'repos/{REPOSITORY}/actions/artifacts/{ARTIFACT}']))
    if (metadata['expired'] or metadata['id'] != ARTIFACT or metadata['name'] != 'core-source-inputs'
            or metadata['digest'] != f'sha256:{ZIP_SHA256}'
            or metadata['workflow_run']['id'] != RUN
            or metadata['workflow_run']['head_sha'] != PRODUCER
            or metadata['workflow_run']['head_branch'] != 'main'):
        raise ValueError('accepted Core proof artifact is unavailable or has changed')
    output.mkdir(parents=True, exist_ok=False)
    archive = output.parent / f'{output.name}.zip'
    with archive.open('xb') as target:
        subprocess.run(['gh', 'api', f'repos/{REPOSITORY}/actions/artifacts/{ARTIFACT}/zip'], stdout=target, check=True)
    if hashlib.sha256(archive.read_bytes()).hexdigest() != ZIP_SHA256:
        raise ValueError('accepted Core proof ZIP digest mismatch')
    pin_path = Path(__file__).resolve().parents[2] / 'connector/rust-core-production-pin.json'
    pin = json.loads(pin_path.read_bytes())
    with zipfile.ZipFile(archive) as source:
        manifest_bytes = source.read('native-core-manifest.json')
        manifest = json.loads(manifest_bytes)
        if manifest['source_sha'] != pin['source_sha'] or manifest['cargo_lock_sha256'] != pin['cargo_lock_sha256']:
            raise ValueError('accepted Core proof differs from the current source pin')
        records = manifest['bundles']
        names = {record['name'] for record in records.values()} | {'native-core-manifest.json'}
        if len(source.namelist()) != len(names) or set(source.namelist()) != names:
            raise ValueError('unexpected archive membership')
        for record in records.values():
            name = record['name']
            if Path(name).name != name or not 0 < record['size'] <= 250_000_000:
                raise ValueError('invalid native archive record')
            content = source.read(name)
            if len(content) != record['size'] or hashlib.sha256(content).hexdigest() != record['sha256']:
                raise ValueError('native archive differs from verified manifest')
            (output / name).write_bytes(content)
        (output / 'native-core-manifest.json').write_bytes(manifest_bytes)
    archive.unlink()
    print(f'Verified accepted Core inputs from run {RUN}, source {pin["source_sha"]}')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', required=True, type=Path)
    fetch(parser.parse_args().output)
