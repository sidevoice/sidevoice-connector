"""Retrieve one exact accepted Mac SEA only as an existing-install compatibility fixture."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import zipfile

ARTIFACT = 11300172651
ZIP_SHA256 = '194d07bebbb039e5c71341d7a95f059f7836c3b115dac72e399adb911a2cd891'
BINARY_SHA256 = '513685cacc1f0833ccefa6c9fcdce98a34ea093191ecfd12bfa930f31e4cf0b3'
REPOSITORY = 'sidevoice/sidevoice-connector'


def fetch(output):
    meta = json.loads(subprocess.check_output(['gh', 'api', f'repos/{REPOSITORY}/actions/artifacts/{ARTIFACT}']))
    if (meta['expired'] or meta['id'] != ARTIFACT or meta['name'] != 'sidevoice-rust-pair-macos-aarch64'
            or meta['digest'] != f'sha256:{ZIP_SHA256}'
            or meta['workflow_run']['id'] != 37193213949
            or meta['workflow_run']['head_sha'] != '435fd315e657a4f1372fc524f773387797195f38'
            or meta['workflow_run']['head_branch'] != 'main'):
        raise ValueError('accepted legacy fixture is unavailable or differs from its provenance')
    archive = output.with_suffix('.zip')
    with archive.open('xb') as target:
        subprocess.run(['gh', 'api', f'repos/{REPOSITORY}/actions/artifacts/{ARTIFACT}/zip'], check=True, stdout=target)
    if hashlib.sha256(archive.read_bytes()).hexdigest() != ZIP_SHA256:
        raise ValueError('legacy fixture ZIP digest mismatch')
    with zipfile.ZipFile(archive) as source:
        if len(source.infolist()) != 1 or source.infolist()[0].file_size != 147249568:
            raise ValueError('unexpected legacy fixture membership')
        binary = source.read(source.infolist()[0])
        if hashlib.sha256(binary).hexdigest() != BINARY_SHA256:
            raise ValueError('legacy fixture binary digest mismatch')
        with output.open('xb') as target:
            target.write(binary)
    output.chmod(0o755)
    archive.unlink()


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', required=True, type=Path)
    fetch(parser.parse_args().output)
