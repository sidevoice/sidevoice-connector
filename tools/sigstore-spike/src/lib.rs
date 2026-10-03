//! Isolated proof of Sidevoice's Core attestation policy. This is not installer code.
use base64::{engine::general_purpose::STANDARD, Engine};
use const_oid::ObjectIdentifier;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sigstore_trust_root::TrustedRoot;
use sigstore_types::{Bundle, SignatureContent};
use sigstore_verify::{IdentityMatcher, VerificationPolicy, VerificationResult, Verifier};
use std::{fs::File, io::{Read, Seek, SeekFrom}, path::Path};
use x509_cert::{der::Decode, Certificate};

pub const CORE_REPOSITORY: &str = "https://github.com/sidevoice/sidevoice-core";
pub const CORE_REPOSITORY_ID: &str = "1399406535";
pub const CORE_ISSUER: &str = "https://token.actions.githubusercontent.com";
pub const CORE_SIGNER: &str = "https://github.com/sidevoice/sidevoice-core/.github/workflows/test.yml@refs/heads/main";
const SLSA: &str = "https://slsa.dev/provenance/v1";
const INTOTO: &str = "https://in-toto.io/Statement/v1";
const PAYLOAD_TYPE: &str = "application/vnd.in-toto+json";
const MAX_BUNDLE: u64 = 1024 * 1024;

type ProofResult<T> = Result<T, String>;
fn refuse<T>(check: &str) -> ProofResult<T> { Err(format!("refused: {check}")) }

#[derive(Clone, Debug)]
pub struct Claims {
    pub issuer: Option<String>,
    pub san: Option<String>,
    pub legacy_issuer: Option<String>,
    pub build_signer: Option<String>,
    pub source: Option<String>,
    pub repository_id: Option<String>,
    pub runner: Option<String>,
    pub build_config: Option<String>,
}

fn oid(number: &str) -> ObjectIdentifier { ObjectIdentifier::new(number).expect("fixed OID") }

/// Inspect only raw extension values and multiplicity; sigstore-verify owns X.509 validation.
pub fn inspect_required_extensions(cert_der: &[u8]) -> ProofResult<String> {
    let cert = Certificate::from_der(cert_der).map_err(|e| format!("certificate parse: {e}"))?;
    let extensions = cert.tbs_certificate.extensions.as_ref().ok_or("missing extensions")?;
    inspect_extension_values(extensions)
}

fn inspect_extension_values(extensions: &[x509_cert::ext::Extension]) -> ProofResult<String> {
    let required = ["1.3.6.1.4.1.57264.1.1", "1.3.6.1.4.1.57264.1.9",
        "1.3.6.1.4.1.57264.1.11", "1.3.6.1.4.1.57264.1.12",
        "1.3.6.1.4.1.57264.1.15", "1.3.6.1.4.1.57264.1.18"];
    for id in required {
        if extensions.iter().filter(|ext| ext.extn_id == oid(id)).count() != 1 {
            return refuse("missing or duplicate required Fulcio extension");
        }
    }
    let raw = extensions.iter().find(|ext| ext.extn_id == oid(required[0])).unwrap().extn_value.as_bytes();
    let issuer = std::str::from_utf8(raw).map_err(|_| "malformed raw legacy issuer")?;
    if issuer != CORE_ISSUER { return refuse("raw legacy issuer"); }
    Ok(issuer.to_owned())
}

/// Validate the original JSON wire spelling before Sigstore's permissive ProtoJSON decoder.
pub fn parse_bundle(json: &[u8]) -> ProofResult<Bundle> {
    if json.len() as u64 > MAX_BUNDLE { return refuse("oversized sidecar"); }
    let wire: Value = serde_json::from_slice(json).map_err(|e| format!("bundle JSON: {e}"))?;
    let envelope = wire.get("dsseEnvelope").ok_or("DSSE envelope required")?;
    let encoded = envelope.get("payload").and_then(Value::as_str).ok_or("DSSE payload missing")?;
    if encoded.is_empty() { return refuse("empty DSSE payload"); }
    let decoded = STANDARD.decode(encoded).map_err(|_| "noncanonical DSSE payload base64")?;
    if STANDARD.encode(&decoded) != encoded { return refuse("noncanonical DSSE payload base64"); }
    let bundle = Bundle::from_json(std::str::from_utf8(json).map_err(|_| "bundle is not UTF-8")?)
        .map_err(|e| format!("bundle format: {e}"))?;
    match &bundle.content {
        SignatureContent::DsseEnvelope(dsse) if dsse.payload_type == PAYLOAD_TYPE => Ok(bundle),
        _ => refuse("DSSE in-toto payload type"),
    }
}

fn at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    path.iter().try_fold(value, |v, key| v.get(key))?.as_str()
}

/// Pure Sidevoice policy; call only on claims and payload obtained from the verified bundle.
pub fn enforce_core_provenance(claims: &Claims, statement: &Value, digest: &str, channel: &str) -> ProofResult<()> {
    let (build_config, workflow_path) = match channel {
        "nightly" => (CORE_SIGNER, ".github/workflows/test.yml"),
        "release" => ("https://github.com/sidevoice/sidevoice-core/.github/workflows/release-please.yml@refs/heads/main", ".github/workflows/release-please.yml"),
        _ => return refuse("unsupported channel"),
    };
    let equals = |actual: &Option<String>, expected: &str| actual.as_deref() == Some(expected);
    if !equals(&claims.issuer, CORE_ISSUER) || !equals(&claims.legacy_issuer, CORE_ISSUER) { return refuse("issuer"); }
    if !equals(&claims.san, CORE_SIGNER) || !equals(&claims.build_signer, CORE_SIGNER) { return refuse("signer workflow"); }
    if !equals(&claims.source, CORE_REPOSITORY) { return refuse("source repository"); }
    if !equals(&claims.repository_id, CORE_REPOSITORY_ID) { return refuse("repository ID"); }
    if !equals(&claims.runner, "github-hosted") { return refuse("runner"); }
    if !equals(&claims.build_config, build_config) { return refuse("build config"); }
    if at(statement, &["_type"]) != Some(INTOTO) { return refuse("statement type"); }
    if at(statement, &["predicateType"]) != Some(SLSA) { return refuse("predicate type"); }
    let workflow = ["predicate", "buildDefinition", "externalParameters", "workflow"];
    let wf = workflow.iter().try_fold(statement, |v, key| v.get(key)).ok_or("workflow missing")?;
    if at(wf, &["repository"]) != Some(CORE_REPOSITORY) || at(wf, &["ref"]) != Some("refs/heads/main")
        || at(wf, &["path"]) != Some(workflow_path) { return refuse("workflow"); }
    if at(statement, &["predicate", "runDetails", "builder", "id"]) != Some(build_config) { return refuse("builder ID"); }
    if at(statement, &["predicate", "buildDefinition", "internalParameters", "github", "repository_id"]) != Some(CORE_REPOSITORY_ID) { return refuse("SLSA repository ID"); }
    if at(statement, &["predicate", "buildDefinition", "internalParameters", "github", "runner_environment"]) != Some("github-hosted") { return refuse("SLSA runner"); }
    let subjects = statement.get("subject").and_then(Value::as_array).ok_or("subject missing")?;
    if subjects.len() != 1 { return refuse("subject count"); }
    let digest_map = subjects[0].get("digest").and_then(Value::as_object).ok_or("subject digest missing")?;
    if digest_map.len() != 1 { return refuse("subject digest algorithms"); }
    let subject_digest = digest_map.get("sha256").and_then(Value::as_str).ok_or("SHA-256 subject missing")?;
    if subject_digest.len() != 64 || !subject_digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) { return refuse("SHA-256 format"); }
    if subject_digest != digest { return refuse("subject digest mismatch"); }
    Ok(())
}

pub fn verify_file(path: &Path, sidecar: &[u8], root: &TrustedRoot, policy: &VerificationPolicy,
    channel: Option<&str>, expected_digest: Option<&str>, expected_size: Option<u64>) -> ProofResult<(String, u64)> {
    let bundle = parse_bundle(sidecar)?;
    let verifier = Verifier::new(root).map_err(|e| format!("verifier: {e}"))?;
    let mut file = File::open(path).map_err(|e| format!("artifact open: {e}"))?;
    let result = verifier.verify_reader(&mut file, &bundle, policy).map_err(|e| format!("cryptographic verification: {e}"))?;
    if !(result.certificate_verified() && result.sct_verified() && result.tlog_verified() && result.identity_policy_checked()) {
        return refuse("verification evidence incomplete");
    }
    file.seek(SeekFrom::Start(0)).map_err(|e| format!("rewind artifact: {e}"))?;
    let mut hash = Sha256::new();
    let mut size = 0;
    let mut block = [0u8; 65536];
    loop {
        let count = file.read(&mut block).map_err(|e| format!("hash artifact: {e}"))?;
        if count == 0 { break; }
        hash.update(&block[..count]);
        size += count as u64;
    }
    let digest = hex::encode(hash.finalize());
    if expected_digest.is_some_and(|expected| expected != digest) { return refuse("manifest digest mismatch"); }
    if expected_size.is_some_and(|expected| expected != size) { return refuse("manifest size mismatch"); }
    if let Some(channel) = channel {
        let cert = bundle.signing_certificate().ok_or("signing certificate missing")?;
        let legacy_issuer = inspect_required_extensions(cert.as_bytes())?;
        let claims = core_claims(&result, legacy_issuer)?;
        let payload = match &bundle.content { SignatureContent::DsseEnvelope(dsse) => dsse.payload.as_bytes(), _ => unreachable!() };
        let statement: Value = serde_json::from_slice(payload).map_err(|e| format!("verified statement JSON: {e}"))?;
        enforce_core_provenance(&claims, &statement, &digest, channel)?;
    }
    Ok((digest, size))
}

fn core_claims(result: &VerificationResult, legacy_issuer: String) -> ProofResult<Claims> {
    let cert = result.certificate().ok_or("certificate result missing")?;
    let ci = &cert.ci_claims;
    Ok(Claims {
        issuer: result.issuer().map(str::to_owned),
        san: result.identity().map(|v| v.as_str().to_owned()),
        legacy_issuer: Some(legacy_issuer),
        build_signer: ci.build_signer_uri.clone(),
        source: ci.source_repository_uri.clone(),
        repository_id: ci.source_repository_identifier.clone(),
        runner: ci.runner_environment.clone(),
        build_config: ci.build_config_uri.clone(),
    })
}

pub fn core_policy() -> VerificationPolicy {
    VerificationPolicy::new(IdentityMatcher::Uri(CORE_SIGNER.to_owned()), CORE_ISSUER)
}

pub fn npm_fixture_policy() -> VerificationPolicy {
    VerificationPolicy::new(IdentityMatcher::Uri("https://github.com/sigstore/sigstore-js/.github/workflows/release.yml@refs/heads/main".to_owned()), CORE_ISSUER)
}

pub fn read_sidecar(path: &Path) -> ProofResult<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path).map_err(|e| format!("sidecar open: {e}"))?.take(MAX_BUNDLE + 1)
        .read_to_end(&mut bytes).map_err(|e| format!("sidecar read: {e}"))?;
    if bytes.len() as u64 > MAX_BUNDLE { return refuse("oversized sidecar"); }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use x509_cert::der::asn1::OctetString;

    const CORE_FIXTURE: &str = include_str!("../../../packages/connector/test/fixtures/r4/core-manifest-nightly.sigstore.json");
    const NPM_FIXTURE: &str = include_str!("../../../packages/connector/test/fixtures/r4/sigstore-5.0.0.sigstore.json");

    fn fixture() -> (Claims, Value, String) {
        let bundle = parse_bundle(CORE_FIXTURE.as_bytes()).unwrap();
        let cert = bundle.signing_certificate().unwrap();
        let info = sigstore_verify::crypto::parse_certificate_info(cert).unwrap();
        let payload = match &bundle.content { SignatureContent::DsseEnvelope(d) => d.payload.as_bytes(), _ => unreachable!() };
        let statement: Value = serde_json::from_slice(payload).unwrap();
        let digest = statement["subject"][0]["digest"]["sha256"].as_str().unwrap().to_owned();
        let claims = Claims {
            issuer: info.issuer,
            san: info.identity.map(|v| v.as_str().to_owned()),
            legacy_issuer: Some(inspect_required_extensions(cert.as_bytes()).unwrap()),
            build_signer: info.ci_claims.build_signer_uri,
            source: info.ci_claims.source_repository_uri,
            repository_id: info.ci_claims.source_repository_identifier,
            runner: info.ci_claims.runner_environment,
            build_config: info.ci_claims.build_config_uri,
        };
        (claims, statement, digest)
    }

    #[test]
    fn core_fixture_policy_and_individual_claim_refusals() {
        let (claims, statement, digest) = fixture();
        enforce_core_provenance(&claims, &statement, &digest, "nightly").unwrap();
        assert!(enforce_core_provenance(&claims, &statement, &digest, "other").is_err());
        let mut changed = claims.clone(); changed.issuer = None;
        assert!(enforce_core_provenance(&changed, &statement, &digest, "nightly").is_err());
        let mut changed = claims.clone(); changed.legacy_issuer = None;
        assert!(enforce_core_provenance(&changed, &statement, &digest, "nightly").is_err());
        let mut changed = claims.clone(); changed.legacy_issuer = Some("wrong".into());
        assert!(enforce_core_provenance(&changed, &statement, &digest, "nightly").is_err());
        for field in 0..6 {
            let mut changed = claims.clone();
            let update = |claims: &mut Claims, value: Option<String>| {
                let target = match field {
                    0 => &mut claims.san, 1 => &mut claims.build_signer,
                    2 => &mut claims.source, 3 => &mut claims.repository_id,
                    4 => &mut claims.runner, _ => &mut claims.build_config,
                };
                *target = value;
            };
            update(&mut changed, Some("wrong".into()));
            assert!(enforce_core_provenance(&changed, &statement, &digest, "nightly").is_err(), "field {field}");
            update(&mut changed, None);
            assert!(enforce_core_provenance(&changed, &statement, &digest, "nightly").is_err(), "missing field {field}");
        }
    }

    #[test]
    fn core_statement_policy_refuses_each_changed_field() {
        let (claims, statement, digest) = fixture();
        let paths: &[&[&str]] = &[
            &["_type"], &["predicateType"],
            &["predicate", "buildDefinition", "externalParameters", "workflow", "repository"],
            &["predicate", "buildDefinition", "externalParameters", "workflow", "ref"],
            &["predicate", "buildDefinition", "externalParameters", "workflow", "path"],
            &["predicate", "runDetails", "builder", "id"],
            &["predicate", "buildDefinition", "internalParameters", "github", "repository_id"],
            &["predicate", "buildDefinition", "internalParameters", "github", "runner_environment"],
        ];
        for path in paths {
            for replacement in [Value::String("wrong".into()), Value::Null] {
                let mut changed = statement.clone();
                let (last, parents) = path.split_last().unwrap();
                let parent = parents.iter().fold(&mut changed, |v, key| &mut v[*key]);
                parent[*last] = replacement;
                assert!(enforce_core_provenance(&claims, &changed, &digest, "nightly").is_err(), "path {path:?}");
            }
        }
        let mut changed = statement.clone();
        changed["subject"].as_array_mut().unwrap().push(statement["subject"][0].clone());
        assert!(enforce_core_provenance(&claims, &changed, &digest, "nightly").is_err());
        for key in ["sha512", "other"] {
            let mut changed = statement.clone();
            changed["subject"][0]["digest"][key] = Value::String("ab".into());
            assert!(enforce_core_provenance(&claims, &changed, &digest, "nightly").is_err());
        }
        for malformed in [digest.to_uppercase(), "a".repeat(63), "z".repeat(64)] {
            let mut changed = statement.clone(); changed["subject"][0]["digest"]["sha256"] = Value::String(malformed);
            assert!(enforce_core_provenance(&claims, &changed, &digest, "nightly").is_err());
        }
    }

    #[test]
    fn raw_legacy_issuer_is_independent_of_modern_issuer() {
        let bundle = parse_bundle(CORE_FIXTURE.as_bytes()).unwrap();
        let cert = Certificate::from_der(bundle.signing_certificate().unwrap().as_bytes()).unwrap();
        let mut extensions = cert.tbs_certificate.extensions.clone().unwrap();
        assert_eq!(inspect_extension_values(&extensions).unwrap(), CORE_ISSUER);
        let legacy = oid("1.3.6.1.4.1.57264.1.1");
        let index = extensions.iter().position(|ext| ext.extn_id == legacy).unwrap();
        let original = extensions[index].clone();
        extensions[index].extn_value = OctetString::new(b"wrong".to_vec()).unwrap();
        assert!(inspect_extension_values(&extensions).is_err());
        extensions[index] = original.clone(); extensions.push(original);
        assert!(inspect_extension_values(&extensions).is_err());
        extensions.pop(); extensions.remove(index);
        assert!(inspect_extension_values(&extensions).is_err());
        let mut extensions = cert.tbs_certificate.extensions.unwrap_or_default();
        let duplicate_ci = extensions.iter().find(|ext| ext.extn_id == oid("1.3.6.1.4.1.57264.1.9")).unwrap().clone();
        extensions.push(duplicate_ci);
        assert!(inspect_extension_values(&extensions).is_err());
    }

    #[test]
    fn wire_shape_and_canonical_base64_are_strict() {
        let base: Value = serde_json::from_str(NPM_FIXTURE).unwrap();
        assert!(parse_bundle(NPM_FIXTURE.as_bytes()).is_ok());
        let changes = [
            ("payload", Value::String("_".into())),
            ("payload", Value::String("".into())),
            ("payloadType", Value::String("wrong".into())),
        ];
        for (key, value) in changes {
            let mut changed = base.clone(); changed["dsseEnvelope"][key] = value;
            assert!(parse_bundle(changed.to_string().as_bytes()).is_err());
        }
        for count in [0, 2] {
            let mut changed = base.clone();
            changed["dsseEnvelope"]["signatures"] = Value::Array(vec![base["dsseEnvelope"]["signatures"][0].clone(); count]);
            assert!(parse_bundle(changed.to_string().as_bytes()).is_err());
        }
        let mut changed = base.clone(); changed.as_object_mut().unwrap().remove("dsseEnvelope");
        assert!(parse_bundle(changed.to_string().as_bytes()).is_err());
        assert!(parse_bundle(&vec![b' '; MAX_BUNDLE as usize + 1]).is_err());
    }
}
