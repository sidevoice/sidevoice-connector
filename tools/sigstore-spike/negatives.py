"""Hosted-only mutation checks. Signed contents are never treated as accepted fixtures."""
import base64
import copy
import json
import pathlib
import subprocess
import sys

binary, snapshot, cache, npm_dir = map(pathlib.Path, sys.argv[1:])
manifest = snapshot / "core-manifest.json"
sidecar = snapshot / "core-manifest.json.sigstore.json"
wire = json.loads(sidecar.read_bytes())
scratch = snapshot / "mutations"
scratch.mkdir(exist_ok=True)


def run(artifact, bundle, channel, accepted):
    result = subprocess.run([str(binary), str(artifact), str(bundle), channel, str(cache), "offline"], capture_output=True, text=True)
    if (result.returncode == 0) != accepted:
        raise AssertionError(f"expected accepted={accepted}: {result.args}\nstdout={result.stdout}\nstderr={result.stderr}")


def changed_base64(encoded):
    raw = bytearray(base64.b64decode(encoded, validate=True))
    raw[-1] ^= 1
    return base64.b64encode(raw).decode()


def mutation(name, change):
    current = copy.deepcopy(wire)
    change(current)
    path = scratch / (name + ".json")
    path.write_text(json.dumps(current))
    run(manifest, path, "nightly", False)
    print("refused", name)


run(manifest, sidecar, "nightly", True)
changed = scratch / "artifact-byte-flip"
payload = bytearray(manifest.read_bytes())
payload[-1] ^= 1
changed.write_bytes(payload)
run(changed, sidecar, "nightly", False)
print("refused artifact-byte-flip")

mutation("payload-byte-flip", lambda b: b["dsseEnvelope"].__setitem__("payload", changed_base64(b["dsseEnvelope"]["payload"])))
mutation("signature-byte-flip", lambda b: b["dsseEnvelope"]["signatures"][0].__setitem__("sig", changed_base64(b["dsseEnvelope"]["signatures"][0]["sig"])))
mutation("certificate-byte-flip", lambda b: b["verificationMaterial"]["certificate"].__setitem__("rawBytes", changed_base64(b["verificationMaterial"]["certificate"]["rawBytes"])))
mutation("rekor-proof", lambda b: b["verificationMaterial"]["tlogEntries"][0]["inclusionProof"].__setitem__("rootHash", changed_base64(b["verificationMaterial"]["tlogEntries"][0]["inclusionProof"]["rootHash"])))
mutation("rekor-checkpoint", lambda b: b["verificationMaterial"]["tlogEntries"][0]["inclusionProof"].__setitem__("checkpoint", {"envelope": "broken checkpoint"}))
mutation("rekor-set", lambda b: b["verificationMaterial"]["tlogEntries"][0]["inclusionPromise"].__setitem__("signedEntryTimestamp", changed_base64(b["verificationMaterial"]["tlogEntries"][0]["inclusionPromise"]["signedEntryTimestamp"])))
mutation("rekor-body", lambda b: b["verificationMaterial"]["tlogEntries"][0].__setitem__("canonicalizedBody", changed_base64(b["verificationMaterial"]["tlogEntries"][0]["canonicalizedBody"])))
mutation("rekor-time", lambda b: b["verificationMaterial"]["tlogEntries"][0].__setitem__("integratedTime", str(int(b["verificationMaterial"]["tlogEntries"][0]["integratedTime"]) + 1)))
mutation("missing-proof", lambda b: b["verificationMaterial"]["tlogEntries"][0].pop("inclusionProof"))
run(manifest, scratch / "absent.json", "nightly", False)
print("refused missing-sidecar")

npm_artifact = npm_dir / "sigstore-5.0.0.tgz"
npm_sidecar = npm_dir / "sigstore-5.0.0.sigstore.json"
run(npm_artifact, npm_sidecar, "npm", True)
identity_result = subprocess.run(
    [str(binary), str(npm_artifact), str(npm_sidecar), "nightly", str(cache), "offline"],
    capture_output=True, text=True,
)
if identity_result.returncode != 1 or "cryptographic verification: identity mismatch:" not in identity_result.stderr:
    raise AssertionError(
        f"expected Sigstore identity refusal: exit={identity_result.returncode}\n"
        f"stdout={identity_result.stdout}\nstderr={identity_result.stderr}"
    )
print("accepted unrelated signer under its own exact policy; Core refusal:", identity_result.stderr.strip())
