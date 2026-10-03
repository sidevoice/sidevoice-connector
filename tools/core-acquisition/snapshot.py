"""Capture one coherent signed nightly Core manifest, macOS arm64 bundle and sidecars."""
import base64
import hashlib
import json
import pathlib
import sys
import urllib.parse
import urllib.request

BASE = "https://github.com/sidevoice/sidevoice-core/releases/download/nightly/"
OUT = pathlib.Path(sys.argv[1])
OUT.mkdir(parents=True, exist_ok=True)
MAX_SMALL = 4 * 1024 * 1024
MAX_ARCHIVE = 1024 * 1024 * 1024


def read(url, limit):
    request = urllib.request.Request(url, headers={"User-Agent": "sidevoice-core-acquisition-ci"})
    with urllib.request.urlopen(request, timeout=120) as response:
        final = urllib.parse.urlparse(response.geturl())
        if final.scheme != "https" or final.hostname not in {
            "github.com",
            "release-assets.githubusercontent.com",
        }:
            raise ValueError(f"unexpected GitHub release redirect: {response.geturl()}")
        body = response.read(limit + 1)
    if len(body) > limit:
        raise ValueError(f"input exceeds size limit: {url}")
    return body


def download(url, path, expected_size):
    request = urllib.request.Request(url, headers={"User-Agent": "sidevoice-core-acquisition-ci"})
    with urllib.request.urlopen(request, timeout=120) as response:
        final = urllib.parse.urlparse(response.geturl())
        if final.scheme != "https" or final.hostname not in {
            "github.com",
            "release-assets.githubusercontent.com",
        }:
            raise ValueError(f"unexpected GitHub release redirect: {response.geturl()}")
        digest = hashlib.sha256()
        size = 0
        with path.open("wb") as target:
            while block := response.read(1024 * 1024):
                size += len(block)
                if size > min(expected_size, MAX_ARCHIVE):
                    raise ValueError("Core archive exceeds signed size limit")
                digest.update(block)
                target.write(block)
    if size != expected_size:
        raise ValueError("Core archive size differs from signed manifest")
    return digest.hexdigest(), size


def statement(bundle):
    wire = json.loads(bundle)
    payload = wire["dsseEnvelope"]["payload"]
    decoded = base64.b64decode(payload, validate=True)
    if base64.b64encode(decoded).decode() != payload:
        raise ValueError("noncanonical Sigstore payload")
    return json.loads(decoded)


for attempt in range(2):
    manifest = read(BASE + "core-manifest.json", 1024 * 1024)
    manifest_sidecar = read(BASE + "core-manifest.json.sigstore.json", MAX_SMALL)
    manifest_sha = hashlib.sha256(manifest).hexdigest()
    manifest_statement = statement(manifest_sidecar)
    if manifest_statement["subject"] != [
        {"name": "core-manifest.json", "digest": {"sha256": manifest_sha}}
    ]:
        if attempt == 0:
            continue
        raise ValueError("nightly manifest moved while reading its sidecar")
    parsed = json.loads(manifest)
    if set(parsed) != {"bundles", "wheel"}:
        raise ValueError("Core producer schema changed")
    bundle = next(
        item for item in parsed["bundles"]
        if item["os"] == "macos" and item["arch"] == "aarch64"
    )
    if not bundle["url"].startswith(BASE):
        raise ValueError("macOS arm64 asset is outside the pinned nightly release")
    asset_name = bundle["url"].rsplit("/", 1)[-1]
    if asset_name != "sidevoice-core-0.1.0-macos-aarch64.tar.zst":
        raise ValueError("unexpected pinned Core filename")
    asset_sidecar = read(bundle["url"] + ".sigstore.json", MAX_SMALL)
    asset_statement = statement(asset_sidecar)
    if asset_statement["subject"] != [
        {"name": asset_name, "digest": {"sha256": bundle["sha256"]}}
    ]:
        if attempt == 0:
            continue
        raise ValueError("nightly bundle sidecar moved")
    archive = OUT / "core-macos-aarch64.tar.zst"
    archive_sha, archive_size = download(bundle["url"], archive, bundle["size"])
    if (
        archive_sha != bundle["sha256"]
        or hashlib.sha256(read(BASE + "core-manifest.json", 1024 * 1024)).hexdigest() != manifest_sha
    ):
        archive.unlink(missing_ok=True)
        if attempt == 0:
            continue
        raise ValueError("nightly moved or archive differs from signed manifest")
    (OUT / "core-manifest.json").write_bytes(manifest)
    (OUT / "core-manifest.json.sigstore.json").write_bytes(manifest_sidecar)
    (OUT / "core-macos-aarch64.tar.zst.sigstore.json").write_bytes(asset_sidecar)
    result = {
        "manifest_sha256": manifest_sha,
        "manifest_sidecar_sha256": hashlib.sha256(manifest_sidecar).hexdigest(),
        "asset_sha256": archive_sha,
        "asset_sidecar_sha256": hashlib.sha256(asset_sidecar).hexdigest(),
        "asset_size": archive_size,
        "asset_url": bundle["url"],
        "source_run": manifest_statement["predicate"]["runDetails"]["metadata"]["invocationId"],
        "source_commit": manifest_statement["predicate"]["buildDefinition"]["resolvedDependencies"][0]["digest"]["gitCommit"],
    }
    (OUT / "snapshot.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))
    break
else:
    raise ValueError("could not capture one coherent Core nightly after one retry")
