"""Read one coherent mutable nightly set into runner-local files; never open the archive."""
import base64
import hashlib
import json
import pathlib
import sys
import urllib.request

BASE = "https://github.com/sidevoice/sidevoice-core/releases/download/nightly/"
OUT = pathlib.Path(sys.argv[1])
OUT.mkdir(parents=True, exist_ok=True)


def read(url, limit):
    with urllib.request.urlopen(urllib.request.Request(url, headers={"User-Agent": "sidevoice-sigstore-spike"}), timeout=60) as response:
        body = response.read(limit + 1)
    if len(body) > limit:
        raise ValueError(f"input exceeds limit: {url}")
    return body


def signed_statement(bundle_bytes):
    wire = json.loads(bundle_bytes)
    return json.loads(base64.b64decode(wire["dsseEnvelope"]["payload"], validate=True))


for attempt in range(2):
    manifest = read(BASE + "core-manifest.json", 1_000_000)
    manifest_bundle = read(BASE + "core-manifest.json.sigstore.json", 1_000_000)
    manifest_sha = hashlib.sha256(manifest).hexdigest()
    manifest_statement = signed_statement(manifest_bundle)
    if manifest_statement["subject"] != [{"name": "core-manifest.json", "digest": {"sha256": manifest_sha}}]:
        if attempt == 0:
            continue
        raise ValueError("nightly manifest moved while reading its sidecar")
    payload = json.loads(manifest)
    asset = next(item for item in payload["bundles"] if item["os"] == "linux" and item["arch"] == "x86_64")
    asset_url = asset["url"]
    if not asset_url.startswith(BASE):
        raise ValueError("unexpected nightly asset URL")
    sidecar = read(asset_url + ".sigstore.json", 1_000_000)
    asset_statement = signed_statement(sidecar)
    if asset_statement["subject"] != [{"name": asset_url.rsplit("/", 1)[-1], "digest": {"sha256": asset["sha256"]}}]:
        if attempt == 0:
            continue
        raise ValueError("nightly asset sidecar moved")
    archive = OUT / "core-linux-x86_64.tar.zst"
    hash_value = hashlib.sha256()
    size = 0
    with urllib.request.urlopen(urllib.request.Request(asset_url, headers={"User-Agent": "sidevoice-sigstore-spike"}), timeout=60) as response, archive.open("wb") as target:
        while block := response.read(1 << 20):
            size += len(block)
            if size > 300_000_000:
                raise ValueError("archive exceeds proof limit")
            hash_value.update(block)
            target.write(block)
    if size != asset["size"] or hash_value.hexdigest() != asset["sha256"] or hashlib.sha256(read(BASE + "core-manifest.json", 1_000_000)).hexdigest() != manifest_sha:
        archive.unlink(missing_ok=True)
        if attempt == 0:
            continue
        raise ValueError("nightly moved or archive bytes differ from signed manifest")
    (OUT / "core-manifest.json").write_bytes(manifest)
    (OUT / "core-manifest.json.sigstore.json").write_bytes(manifest_bundle)
    (OUT / "core-linux-x86_64.tar.zst.sigstore.json").write_bytes(sidecar)
    metadata = {
        "manifest_sha256": manifest_sha,
        "manifest_sidecar_sha256": hashlib.sha256(manifest_bundle).hexdigest(),
        "asset_sha256": asset["sha256"],
        "asset_sidecar_sha256": hashlib.sha256(sidecar).hexdigest(),
        "asset_size": size,
        "asset_url": asset_url,
        "source_run": manifest_statement["predicate"]["runDetails"]["metadata"]["invocationId"],
        "source_commit": manifest_statement["predicate"]["buildDefinition"]["resolvedDependencies"][0]["digest"]["gitCommit"],
    }
    (OUT / "snapshot.json").write_text(json.dumps(metadata, indent=2) + "\n")
    print(json.dumps(metadata, indent=2))
    break
else:
    raise ValueError("could not snapshot a coherent nightly after one retry")
