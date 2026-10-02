# R4-b artifact handoff for Desktop

This handoff describes the connector producer output. It does not change Desktop. The workflow keeps PR and
non-main dispatch builds manifestless and test-only. On protected `main`, after all three native SEA test jobs pass,
it uploads the macOS arm64 executable, signs that exact executable with GitHub's public-good Sigstore attestation,
verifies the bundle, and emits a pin from GitHub's run artifact metadata. A ready production pin still depends on the
genuine R4-a manifest, its Sigstore bundle, and platform core assets.

## Pin and artifact records

- `asset_url` is `https://api.github.com/repos/sidevoice/sidevoice-connector/actions/artifacts/<artifact_id>/zip`.
  The executable archive contains exactly one root entry named `sidevoice`; `executable_sha256` and
  `executable_size` describe the uncompressed executable bytes.
- `provenance.artifact_name` is `sidevoice-connector-macos-aarch64-r4b`.
- `provenance.sidecars` contains one record named `sidevoice-provenance.zip`. Its `url` is the canonical artifact ZIP
  route for `sidevoice-connector-macos-aarch64-r4b-provenance`; `sha256` and `size` describe the ZIP bytes from the
  official run artifact record (`digest` and `size_in_bytes`), not the JSON bundle inside it.
- The provenance ZIP contains exactly one root entry named `sidevoice.sigstore.json`. This is the unmodified bundle
  produced by `actions/attest@v4`; CI verifies it against the downloaded `sidevoice` executable and requires the
  connector repository, `r4-sea.yml` signer workflow, `refs/heads/main`, the exact source commit, and a GitHub-hosted
  runner. The connector does not synthesize a provenance bundle.
- `core_manifest_sidecars` continues to refer to the raw core release sidecar bytes. Its hash and size are for the
  downloaded JSON file, not an Actions ZIP.

The artifact metadata is read from `GET /repos/sidevoice/sidevoice-connector/actions/runs/<run_id>/artifacts`. The
producer checks each selected record's ID, exact name, canonical `archive_download_url`, digest, size, non-expired
state, run ID, repository ID, and source commit before it emits the pin. GitHub documents the archive download as
`GET /repos/{owner}/{repo}/actions/artifacts/{artifact_id}/zip`; the previous `/actions/runs/{run_id}/artifacts/...`
URL shape is not the download route.

## Desktop packaging fetch and verification

Fetch these artifacts in the trusted Desktop packaging workflow, then bundle the verified executable in the app. Do
not fetch Actions artifacts from the shipped app or require credentials on the user's machine.

1. In the trusted packaging job, create a short-lived GitHub App installation token scoped to
   `sidevoice/sidevoice-connector` with repository permission `Actions: read`. Keep it out of untrusted PR workflows
   and do not expose it to the connector or other PR-controlled code. A fine-grained token with the same single-repo
   permission can be used for a supervised local packaging run.
2. Read the artifact record for the pinned run and verify its repository ID, run ID, `head_sha == connector_sha`, exact
   artifact name, `expired == false`, canonical `archive_download_url`, `digest`, and `size_in_bytes`.
3. Request the canonical API ZIP URL with the bearer token and `Accept: application/vnd.github+json`. Handle the 302
   explicitly: validate the HTTPS redirect target is on GitHub's Actions blob storage host
   (`*.blob.core.windows.net`), then fetch the signed URL without forwarding the GitHub Authorization header. Bound
   archive bytes by the pinned/API size and verify the artifact ZIP SHA-256 before extraction.
4. For the executable ZIP, accept exactly root `sidevoice`, verify its SHA-256 and byte size against the pin, then
   check its native arm64 format, ad-hoc signature, `--version --json`, and `metadata --json` against the pin.
5. For `sidevoice-provenance.zip`, verify its ZIP digest and size against `provenance.sidecars[0]`, extract only root
   `sidevoice.sigstore.json`, and verify the bundle cryptographically against the executable digest using Sigstore
   public-good trust material. Require repository `sidevoice/sidevoice-connector`, signer workflow
   `sidevoice/sidevoice-connector/.github/workflows/r4-sea.yml`, source ref `refs/heads/main`, source digest equal to
   the pin's `connector_sha`, and reject self-hosted runners. The equivalent CI check is:

   ```sh
   gh attestation verify sidevoice \
     --repo sidevoice/sidevoice-connector \
     --bundle sidevoice.sigstore.json \
     --signer-workflow github.com/sidevoice/sidevoice-connector/.github/workflows/r4-sea.yml \
     --source-ref refs/heads/main \
     --source-digest "$PIN_CONNECTOR_SHA" \
     --deny-self-hosted-runners
   ```

The Actions API requires `Actions: read` for fine-grained access and returns a short-lived redirect. Do not follow it
while carrying the GitHub bearer token; the redirect is a signed blob URL. The artifact API documents a 90-day
maximum retention for public repository artifacts. These are temporary packaging inputs, not a durable release
source.

## Desktop consumer changes still needed

- Accept `/actions/artifacts/<artifact_id>/zip` and bind the artifact record to the pinned run using the official run
  artifacts API. The current Desktop JS and Rust validators still expect the obsolete `/actions/runs/<run_id>/artifacts`
  route.
- Add the trusted packaging fetch step and short-lived `Actions: read` credential handling above. Current Desktop
  `download()` performs an unauthenticated follow-redirect fetch and does not explicitly prevent authorization from
  crossing the API redirect.
- Treat connector provenance sidecar records as ZIPs: verify ZIP digest/size, extract the single root bundle, and
  verify the Sigstore attestation against the executable. Hash-checking the downloaded ZIP alone does not validate its
  signed claim. Continue verifying the core release sidecar as raw JSON.
- Preserve the connector pin field names and check `provenance.sidecars` as a nonempty array. The producer now supplies
  that array with the real attestation artifact record.

No release or merge has been made. PR/fixture coverage is not genuine R4-a integration evidence.
