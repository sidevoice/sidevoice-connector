//! The connector on npm: one package per target and the launcher `sidevoice` (the esbuild pattern).
//!
//! - `@sidevoice/sidevoice-<os>-<cpu>` (`os`, `cpu` and on Linux `libc` set, so npm installs it only on its own
//!   platform): the target's release archive unpacked as it is (`bin/sidevoice-connector`, `connector.json`, the
//!   core archive under `core/`, `LICENSE`, `notices/`), plus its `package.json` and a README. The connector finds its
//!   package by its own path (the directory above its `bin/`), so it runs from `node_modules` as from the archive.
//! - `sidevoice`: `optionalDependencies` on every platform package at exactly its own version, and the one script
//!   `bin/sidevoice.js` (`xtask/npm/sidevoice.js`), which runs the installed platform package's binary.
//!
//! Every package carries the connector's version. Commands:
//!
//! - `npm ARCHIVE...`: the packages of those release archives and the launcher, packed into [`OUT_DIR`].
//! - `npm-smoke`: install the launcher and this machine's package from [`OUT_DIR`] into a temporary prefix, as a
//!   person would, and run `npx sidevoice --version --json`: it must be that package's connector.
//! - `npm-publish TAG`: the packages of the GitHub release TAG's assets (read back from the release and checked
//!   against its `SHA256SUMS` and attestation), published by trusted publishing (OIDC) only: the platform packages
//!   first, then the launcher as a staged version a maintainer approves on npmjs.com.
//!
//! npm is the pinned [`NPM_VERSION`], installed into `target/npm-cli`, run with a configuration of its own: no
//! `.npmrc` and no token from the environment can take part.

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

use crate::archive::unpack_checked;
use crate::manifest::published_name;
use crate::publish::{gh, signer, verify_attestation};
use crate::util::*;
use crate::{target_names, Result, INVENTORY, TARGETS};

/// Where `cargo xtask npm` leaves the packed packages (emptied first).
pub(crate) const OUT_DIR: &str = "target/npm";
/// The npm CLI every npm step runs: trusted publishing needs 11.5.1 or later, staged publishing `npm stage`.
/// Bumped by hand, like any pin.
pub(crate) const NPM_VERSION: &str = "11.21.0";
const NPM_DIR: &str = "target/npm-cli";
/// The oldest Node.js npm's trusted publishing supports.
const PUBLISH_NODE: [u64; 3] = [22, 14, 0];

const LAUNCHER: &str = "sidevoice";
/// The dist-tag of every platform package: never `latest` or `next`, so installing one by name resolves nothing new;
/// only the launcher, which pins its exact version, brings it.
pub(crate) const PLATFORM_DIST_TAG: &str = "platform";
const PLATFORM_PREFIX: &str = "@sidevoice/sidevoice-";
const LAUNCHER_JS: &str = include_str!("../npm/sidevoice.js");
const LAUNCHER_BIN: &str = "bin/sidevoice.js";
const REPOSITORY: &str = "sidevoice/sidevoice-connector";
const REGISTRY: &str = "https://registry.npmjs.org";

/// A release target's npm `os` and `cpu` ([`crate::TARGETS`]).
pub(crate) fn platform(target: &str) -> Result<(&'static str, &'static str)> {
    TARGETS
        .iter()
        .find(|known| known.name == target)
        .map(|known| (known.npm_os, known.npm_cpu))
        .ok_or_else(|| format!("no npm platform for {target}"))
}

/// The platform package of a release target, `@sidevoice/sidevoice-<os>-<cpu>`.
pub(crate) fn platform_package(target: &str) -> Result<String> {
    let (os, cpu) = platform(target)?;
    Ok(format!("{PLATFORM_PREFIX}{os}-{cpu}"))
}

/// The file `npm pack` writes for a package.
pub(crate) fn tarball_name(name: &str, version: &str) -> String {
    format!(
        "{}-{version}.tgz",
        name.trim_start_matches('@').replace('/', "-")
    )
}

/// The dist-tag the launcher is published under: `latest` for `X.Y.Z`, `next` for a release candidate
/// `X.Y.Z-rc.N`. Anything else is refused. (The platform packages always go under [`PLATFORM_DIST_TAG`].)
pub(crate) fn dist_tag(version: &str) -> Result<&'static str> {
    let numbers = |text: &str, count: usize| {
        let parts: Vec<&str> = text.split('.').collect();
        parts.len() == count
            && parts.iter().all(|part| {
                !part.is_empty()
                    && part.bytes().all(|byte| byte.is_ascii_digit())
                    && (*part == "0" || !part.starts_with('0'))
            })
    };
    match version.split_once('-') {
        None if numbers(version, 3) => Ok("latest"),
        Some((release, candidate))
            if numbers(release, 3)
                && candidate
                    .strip_prefix("rc.")
                    .is_some_and(|number| numbers(number, 1)) =>
        {
            Ok("next")
        }
        _ => Err(format!(
            "{version} is neither a release X.Y.Z nor a release candidate X.Y.Z-rc.N"
        )),
    }
}

fn common_fields(name: &str, version: &str, description: &str) -> Value {
    json!({"name": name, "version": version, "description": description, "license": "Apache-2.0",
           "homepage": format!("https://github.com/{REPOSITORY}#readme"),
           "bugs": {"url": format!("https://github.com/{REPOSITORY}/issues")},
           // npm checks provenance against this: it must be the repository the workflow runs in. No `directory`:
           // the packages are generated from the release archives, not a directory of the repository.
           "repository": {"type": "git", "url": format!("git+https://github.com/{REPOSITORY}.git")}})
}

/// `<os>/<cpu>` of a target, as npm names them.
fn platform_label(target: &str) -> Result<String> {
    let (os, cpu) = platform(target)?;
    Ok(format!("{os}/{cpu}"))
}

/// `package.json` of a target's platform package: no `bin`, nothing to run by itself.
pub(crate) fn platform_manifest(target: &str, version: &str) -> Result<Value> {
    let (os, cpu) = platform(target)?;
    let name = platform_package(target)?;
    let mut manifest = common_fields(
        &name,
        version,
        &format!(
            "Platform binary for sidevoice ({}). Do not install directly: install sidevoice.",
            platform_label(target)?
        ),
    );
    manifest["os"] = json!([os]);
    manifest["cpu"] = json!([cpu]);
    if os == "linux" {
        // The binary links against glibc; on musl npm skips the package and the launcher says why.
        manifest["libc"] = json!(["glibc"]);
    }
    Ok(manifest)
}

/// `package.json` of the launcher: every platform package at exactly this version (no range, so the launcher a
/// maintainer approves fixes every byte it runs).
pub(crate) fn launcher_manifest(version: &str) -> Result<Value> {
    let mut manifest = common_fields(
        LAUNCHER,
        version,
        "Give your coding agent a voice: Sidevoice turns your Claude Code, Codex or Cursor conversation into a voice \
         call. Run `npx sidevoice install` on the machine where your agents run.",
    );
    let mut dependencies = serde_json::Map::new();
    for target in target_names() {
        dependencies.insert(platform_package(target)?, version.into());
    }
    manifest["keywords"] = json!([
        "sidevoice",
        "voice",
        "speech",
        "mcp",
        "coding-agent",
        "claude-code",
        "codex",
        "cursor",
        "cli"
    ]);
    manifest["bin"] = json!({LAUNCHER: LAUNCHER_BIN});
    manifest["optionalDependencies"] = dependencies.into();
    manifest["engines"] = json!({"node": ">=18"});
    Ok(manifest)
}

fn platform_readme(target: &str, version: &str) -> Result<String> {
    Ok(format!(
        "# {}\n\nPlatform binary for [`sidevoice`](https://www.npmjs.com/package/sidevoice) ({}), version {version}. \
         Do not install directly — install `sidevoice`:\n\n```sh\nnpx sidevoice install\n```\n\nIt holds the \
         Sidevoice connector built for this platform and the Sidevoice core it installs. Source and documentation: \
         https://github.com/{REPOSITORY}\n",
        platform_package(target)?,
        platform_label(target)?
    ))
}

fn launcher_readme(version: &str) -> Result<String> {
    let platforms: Vec<String> = TARGETS
        .iter()
        .map(|target| match target.npm_os {
            "darwin" => format!("- macOS on {} (`darwin-{}`)", mac_cpu(target.npm_cpu), target.npm_cpu),
            "linux" => format!(
                "- Linux on {} (`linux-{}`) with glibc {} or newer (Debian 10, Ubuntu 20.04, RHEL 8 and later; \
                 not musl-based distributions such as Alpine)",
                target.npm_cpu,
                target.npm_cpu,
                crate::glibc::FLOOR
            ),
            os => format!("- {os} on {}", target.npm_cpu),
        })
        .collect();
    Ok(format!(
        "# sidevoice\n\nGive your coding agent a voice. Sidevoice turns the conversation you already have with your \
         agent (Claude Code, Codex, Cursor) into a voice call: it keeps its context and keeps writing as usual, \
         speaks its replies, and you answer by voice.\n\n## Install\n\nOn the machine where your agents run:\n\n\
         ```sh\nnpx sidevoice install\n```\n\nThen pair the Sidevoice app with `npx sidevoice pair-device`. A release \
         candidate, when there is one: `npx sidevoice@next install`.\n\n## Requirements\n\nOne of these platforms:\n\n{}\n\nand Node.js 18 or newer, \
         for `npx`\n\nThis package ({version}) is a small launcher: npm installs beside it the connector built for \
         your machine (`@sidevoice/sidevoice-<os>-<cpu>`, with the core inside), and the `sidevoice` command runs \
         it. Installing with optional dependencies omitted leaves it nothing to run.\n\n## More\n\nSource, \
         documentation and issues: https://github.com/{REPOSITORY}\n\nApache-2.0. Sidevoice is a trademark; see \
         TRADEMARKS.md in the repository.\n",
        platforms.join("\n")
    ))
}

fn mac_cpu(cpu: &str) -> &str {
    match cpu {
        "arm64" => "Apple silicon",
        "x64" => "Intel",
        other => other,
    }
}

/// One packed package.
pub(crate) struct Packed {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) tarball: PathBuf,
    /// npm's SHA-1 of the tarball, what the registry reports as `dist.shasum`.
    pub(crate) shasum: String,
}

/// The pinned npm, run with a configuration of its own (empty user and global config, its own cache) and none of
/// the environment's npm settings or tokens.
fn npm_command(cli: &str) -> Result<Command> {
    let dir = repo().join(NPM_DIR);
    let script = dir.join("node_modules/npm/bin").join(cli);
    let (user, global) = (dir.join("user-npmrc"), dir.join("global-npmrc"));
    let mut command = Command::new("node");
    command.arg(&script);
    for (key, _) in env::vars_os() {
        let key = key.to_string_lossy();
        if key.to_ascii_lowercase().starts_with("npm_config_")
            || key == "NODE_AUTH_TOKEN"
            || key == "NPM_TOKEN"
        {
            command.env_remove(key.as_ref());
        }
    }
    command
        .env("npm_config_userconfig", &user)
        .env("npm_config_globalconfig", &global)
        .env("npm_config_cache", repo().join("target/npm-cache"))
        .env("npm_config_registry", REGISTRY)
        .env("npm_config_update_notifier", "false")
        .env("npm_config_fund", "false")
        .env("npm_config_audit", "false");
    Ok(command)
}

/// Runs the pinned npm (`cli`: `npm-cli.js` or `npx-cli.js`) and returns its standard output.
fn npm(cli: &str, args: &[&str], dir: Option<&Path>) -> Result<String> {
    let mut command = npm_command(cli)?;
    command.args(args);
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    let result = command.output().map_err(|error| format!("node: {error}"))?;
    if !result.status.success() {
        return Err(format!(
            "npm {}: {}{}",
            args.join(" "),
            String::from_utf8_lossy(&result.stderr),
            String::from_utf8_lossy(&result.stdout)
        ));
    }
    String::from_utf8(result.stdout).map_err(|_| "npm: output is not UTF-8".into())
}

/// Installs the pinned npm into `target/npm-cli` unless it is there.
fn ensure_npm() -> Result<()> {
    let dir = repo().join(NPM_DIR);
    let installed = fs::read(dir.join("node_modules/npm/package.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    if installed.as_ref().map(|manifest| &manifest["version"]) != Some(&json!(NPM_VERSION)) {
        mkdir(&dir)?;
        output(
            "npm",
            &[
                "install",
                "--prefix",
                path_str(&dir)?,
                "--cache",
                path_str(&repo().join("target/npm-cache"))?,
                "--no-audit",
                "--no-fund",
                "--no-save",
                &format!("npm@{NPM_VERSION}"),
            ],
            None,
        )?;
    }
    for config in ["user-npmrc", "global-npmrc"] {
        write(&dir.join(config), b"")?;
    }
    let version = npm("npm-cli.js", &["--version"], None)?;
    if version.trim() != NPM_VERSION {
        return Err(format!(
            "the pinned npm reports {version:?}, not {NPM_VERSION}"
        ));
    }
    Ok(())
}

/// `npm pack` of the package in `dir` into `out`; the files it packed must be exactly `expected`.
fn pack(dir: &Path, out: &Path, expected: &[String]) -> Result<Packed> {
    let report = parse_json(
        npm(
            "npm-cli.js",
            &["pack", "--json", "--pack-destination", path_str(out)?],
            Some(dir),
        )?
        .as_bytes(),
        "npm pack --json",
    )?;
    let report = &report[0];
    let name = report["name"].as_str().unwrap_or("").to_string();
    let version = report["version"].as_str().unwrap_or("").to_string();
    let mut packed: Vec<String> = report["files"]
        .as_array()
        .ok_or("npm pack --json lists no files")?
        .iter()
        .filter_map(|file| file["path"].as_str().map(str::to_string))
        .collect();
    packed.sort();
    let mut wanted = expected.to_vec();
    wanted.sort();
    if packed != wanted {
        return Err(format!(
            "npm packed {name} with other files than its own: {packed:?}, expected {wanted:?}"
        ));
    }
    let filename = tarball_name(&name, &version);
    if report["filename"] != filename.as_str() {
        return Err(format!(
            "npm pack wrote {}, not {filename}",
            report["filename"]
        ));
    }
    Ok(Packed {
        tarball: out.join(&filename),
        shasum: report["shasum"].as_str().unwrap_or("").to_string(),
        name,
        version,
    })
}

/// The platform package of every archive (checked as `manifest` checks them, one version, at most one per target)
/// and the launcher, packed into `out`: platform packages first, the launcher last.
pub(crate) fn build(archives: &[&Path], out: &Path) -> Result<Vec<Packed>> {
    ensure_npm()?;
    let work = TempDir::new("sidevoice-connector-npm")?;
    let mut version: Option<String> = None;
    let mut targets = Vec::new();
    let mut packed = Vec::new();
    for (index, archive) in archives.iter().enumerate() {
        let unpacked = work.0.join(index.to_string());
        let (root, inventory) = unpack_checked(archive, &unpacked)?;
        let target = inventory["target"].as_str().unwrap_or("").to_string();
        let this = inventory["version"].as_str().unwrap_or("").to_string();
        if version.get_or_insert_with(|| this.clone()) != &this {
            return Err(format!(
                "{} is version {this}, another archive {}",
                archive.display(),
                version.unwrap_or_default()
            ));
        }
        if targets.contains(&target) {
            return Err(format!("two archives for {target}"));
        }
        let manifest = platform_manifest(&target, &this)?;
        write(&root.join("package.json"), &pretty(&manifest))?;
        write(
            &root.join("README.md"),
            platform_readme(&target, &this)?.as_bytes(),
        )?;
        // Exactly the archive's files (its inventory and every file it lists), and the two npm needs.
        let mut expected: Vec<String> = inventory["files"]
            .as_array()
            .ok_or("inventory files")?
            .iter()
            .filter_map(|file| file["name"].as_str().map(str::to_string))
            .collect();
        expected.extend([INVENTORY.into(), "package.json".into(), "README.md".into()]);
        packed.push(pack(&root, out, &expected)?);
        targets.push(target);
    }
    let version = version.ok_or("no archive")?;

    let launcher = work.0.join(LAUNCHER);
    mkdir(&launcher.join("bin"))?;
    write(
        &launcher.join("package.json"),
        &pretty(&launcher_manifest(&version)?),
    )?;
    write(&launcher.join(LAUNCHER_BIN), LAUNCHER_JS.as_bytes())?;
    chmod(&launcher.join(LAUNCHER_BIN), 0o755)?;
    write(
        &launcher.join("README.md"),
        launcher_readme(&version)?.as_bytes(),
    )?;
    write(&launcher.join("LICENSE"), &read(&repo().join("LICENSE"))?)?;
    let files = ["package.json", LAUNCHER_BIN, "README.md", "LICENSE"].map(String::from);
    packed.push(pack(&launcher, out, &files)?);
    Ok(packed)
}

fn pretty(value: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(value).expect("JSON values serialize");
    bytes.push(b'\n');
    bytes
}

/// `cargo xtask npm ARCHIVE...`
pub(crate) fn npm_packages(archives: &[&Path]) -> Result<()> {
    let out = repo().join(OUT_DIR);
    let _ = fs::remove_dir_all(&out);
    mkdir(&out)?;
    for packed in build(archives, &out)? {
        println!(
            "{}",
            json!({"name": packed.name, "version": packed.version, "tarball": path_str(&packed.tarball)?,
                   "shasum": packed.shasum})
        );
    }
    Ok(())
}

/// `cargo xtask npm-smoke`: the launcher and this machine's package from [`OUT_DIR`], installed as a person
/// installs them, must run this package's connector, pass its exit status through, and refuse another platform.
pub(crate) fn smoke() -> Result<()> {
    ensure_npm()?;
    let out = repo().join(OUT_DIR);
    let version = connector_version()?;
    let target = host_target()?;
    let name = platform_package(target)?;
    let launcher = out.join(tarball_name(LAUNCHER, &version));
    let platform = out.join(tarball_name(&name, &version));
    // A path with a space, as a home directory may have.
    let prefix = TempDir::new("sidevoice npm smoke")?;
    npm(
        "npm-cli.js",
        &[
            "install",
            "--prefix",
            path_str(&prefix.0)?,
            path_str(&launcher)?,
            path_str(&platform)?,
        ],
        None,
    )?;

    let installed = prefix.0.join("node_modules").join(&name);
    let inventory = parse_json(&read(&installed.join(INVENTORY))?, INVENTORY)?;
    let identity = parse_json(
        npm(
            "npx-cli.js",
            &["sidevoice", "--version", "--json"],
            Some(&prefix.0),
        )?
        .as_bytes(),
        "npx sidevoice --version --json",
    )?;
    for field in ["version", "target", "source_sha"] {
        if identity[field] != inventory[field] {
            return Err(format!(
                "npx sidevoice --version --json says {field} is {}, {name} carries {}",
                identity[field], inventory[field]
            ));
        }
    }
    if identity["version"] != version.as_str() {
        return Err(format!(
            "npx sidevoice runs {}, not {version}",
            identity["version"]
        ));
    }

    // The exit status passes through: an unknown option fails the connector, and the launcher, the same way.
    let script = prefix
        .0
        .join("node_modules")
        .join(LAUNCHER)
        .join(LAUNCHER_BIN);
    let binary = installed.join(inventory["entrypoint"].as_str().unwrap_or(""));
    let direct = Command::new(&binary)
        .arg("--no-such-option")
        .output()
        .map_err(|error| format!("{}: {error}", binary.display()))?;
    let launched = Command::new("node")
        .arg(&script)
        .arg("--no-such-option")
        .output()
        .map_err(|error| format!("node: {error}"))?;
    if direct.status.code().is_none_or(|code| code == 0)
        || launched.status.code() != direct.status.code()
    {
        return Err(format!(
            "the launcher exits with {:?} where the connector exits with {:?}",
            launched.status.code(),
            direct.status.code()
        ));
    }

    // Another platform: a clear refusal, naming what is supported.
    let preload = prefix.0.join("other-platform.js");
    write(
        &preload,
        b"Object.defineProperty(process, 'platform', {value: 'win32'});\n\
          Object.defineProperty(process, 'arch', {value: 'x64'});\n",
    )?;
    let refused = Command::new("node")
        .arg("--require")
        .arg(&preload)
        .arg(&script)
        .arg("--version")
        .output()
        .map_err(|error| format!("node: {error}"))?;
    let message = String::from_utf8_lossy(&refused.stderr);
    if refused.status.code() != Some(1)
        || !message.contains("win32-x64 is not supported")
        || !message.contains("darwin-arm64")
    {
        return Err(format!(
            "on an unsupported platform the launcher exits {:?} saying {message:?}",
            refused.status.code()
        ));
    }
    println!(
        "{}",
        json!({"installed": [LAUNCHER, name], "identity": identity, "exit_status_passes": direct.status.code(),
               "unsupported_platform": message.trim()})
    );
    Ok(())
}

/// The workflow npm's trusted publishing sees: the top-level one (`release-please.yml` when it calls
/// `release.yml`), from `GITHUB_WORKFLOW_REF`.
fn calling_workflow() -> String {
    env::var("GITHUB_WORKFLOW_REF")
        .ok()
        .and_then(|reference| {
            let path = reference.split('@').next()?.to_string();
            path.rsplit('/').next().map(str::to_string)
        })
        .unwrap_or_else(|| "(unknown)".into())
}

/// A request whose secret header goes to curl on its standard input, never on its command line. Returns the HTTP
/// status and the body.
fn curl_secret(url: &str, method: &str, bearer: &str) -> Result<(u16, Vec<u8>)> {
    let mut child = Command::new("curl")
        .args(["--silent", "--show-error", "--retry", "3", "--config", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("curl: {error}"))?;
    let config = format!(
        "url = \"{url}\"\nrequest = \"{method}\"\nheader = \"Accept: application/json\"\n\
         header = \"Authorization: Bearer {bearer}\"\nwrite-out = \"\\n%{{http_code}}\"\n"
    );
    child
        .stdin
        .take()
        .expect("piped")
        .write_all(config.as_bytes())
        .map_err(|error| format!("curl: {error}"))?;
    let result = child
        .wait_with_output()
        .map_err(|error| format!("curl: {error}"))?;
    if !result.status.success() {
        return Err(format!(
            "{method} {url}: {}",
            String::from_utf8_lossy(&result.stderr)
        ));
    }
    let text = result.stdout;
    let split = text.iter().rposition(|byte| *byte == b'\n').unwrap_or(0);
    let status = String::from_utf8_lossy(&text[split..])
        .trim()
        .parse()
        .map_err(|_| format!("{method} {url}: no HTTP status"))?;
    Ok((status, text[..split].to_vec()))
}

/// Before anything is published: the job's OIDC token must be exchangeable for an npm token for every package, as
/// `npm publish` exchanges it. A package without a trusted publisher for this repository and workflow fails here,
/// naming what to configure, and nothing is published.
fn check_trusted_publishing(names: &[&str]) -> Result<()> {
    let (Ok(request_url), Ok(request_token)) = (
        env::var("ACTIONS_ID_TOKEN_REQUEST_URL"),
        env::var("ACTIONS_ID_TOKEN_REQUEST_TOKEN"),
    ) else {
        return Err("no OIDC token: npm packages are published only by trusted publishing, from GitHub Actions \
                    with `permissions: id-token: write` (on the calling workflow too); never with a token"
            .into());
    };
    let audience = format!("npm:{}", REGISTRY.trim_start_matches("https://"));
    let (status, body) = curl_secret(
        &format!("{request_url}&audience={audience}"),
        "GET",
        &request_token,
    )?;
    let id_token = parse_json(&body, "the OIDC token response")?["value"]
        .as_str()
        .filter(|_| status == 200)
        .map(str::to_string)
        .ok_or_else(|| format!("GitHub gave no OIDC token (HTTP {status})"))?;
    let workflow = calling_workflow();
    let mut refused = Vec::new();
    for name in names {
        let url = format!(
            "{REGISTRY}/-/npm/v1/oidc/token/exchange/package/{}",
            name.replace('/', "%2f")
        );
        let (status, body) = curl_secret(&url, "POST", &id_token)?;
        let answer = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
        if !(200..300).contains(&status) || answer["token"].as_str().is_none() {
            refused.push(format!(
                "{name} (HTTP {status}: {})",
                answer["message"].as_str().unwrap_or("no message")
            ));
        }
    }
    if !refused.is_empty() {
        return Err(format!(
            "trusted publishing is not configured for {}. On npmjs.com, each package → Settings → Trusted publisher: \
             GitHub Actions, repository {REPOSITORY}, workflow filename {workflow} (the workflow that started this \
             run; npm checks it, not a workflow it calls), no environment; publishing allowed for the platform \
             packages, staged publishing for `{LAUNCHER}`. Nothing was published.",
            refused.join(", ")
        ));
    }
    println!("trusted publishing: every package accepts {REPOSITORY} {workflow}");
    Ok(())
}

fn node_version() -> Result<[u64; 3]> {
    let text = output("node", &["--version"], None)?;
    let parts: Vec<u64> = text
        .trim()
        .trim_start_matches('v')
        .split('.')
        .map(|part| part.parse().unwrap_or(0))
        .collect();
    match parts.as_slice() {
        [major, minor, patch] => Ok([*major, *minor, *patch]),
        _ => Err(format!("node --version: {text:?}")),
    }
}

/// What the registry holds for `name@version`: its `dist.shasum`, or `None` when no such version is published.
fn published_shasum(name: &str, version: &str) -> Result<Option<String>> {
    match npm(
        "npm-cli.js",
        &[
            "view",
            &format!("{name}@{version}"),
            "dist.shasum",
            "--json",
        ],
        None,
    ) {
        Ok(text) if text.trim().is_empty() => Ok(None),
        Ok(text) => Ok(parse_json(text.as_bytes(), "npm view")?
            .as_str()
            .map(str::to_string)),
        Err(error) if error.contains("E404") => Ok(None),
        Err(error) => Err(error),
    }
}

/// `cargo xtask npm-publish TAG`
pub(crate) fn publish(tag: &str) -> Result<()> {
    let version = tag.strip_prefix('v').ok_or_else(|| {
        format!("{tag} is not a vX.Y.Z release; the nightly is never published to npm")
    })?;
    let dist_tag = dist_tag(version)?;
    if connector_version()? != version {
        return Err(format!(
            "Cargo.toml says {}, the release is {tag}",
            connector_version()?
        ));
    }
    let node = node_version()?;
    if node < PUBLISH_NODE {
        return Err(format!(
            "trusted publishing needs Node.js {}.{}.{} or later; this is {}.{}.{}",
            PUBLISH_NODE[0], PUBLISH_NODE[1], PUBLISH_NODE[2], node[0], node[1], node[2]
        ));
    }
    let repository = env::var("GH_REPO").map_err(|_| "GH_REPO is not set")?;

    // Exactly what the GitHub release published: its archives, read back and checked against its SHA256SUMS and its
    // attestation.
    let assets = TempDir::new("sidevoice-connector-npm-assets")?;
    gh(&[
        "release",
        "download",
        tag,
        "--dir",
        path_str(&assets.0)?,
        "--pattern",
        "*.tar.zst",
        "--pattern",
        "SHA256SUMS",
        "--pattern",
        "attestation.sigstore.json",
    ])?;
    let sums = parse_sums(&read(&assets.0.join("SHA256SUMS"))?)?;
    let mut archives = Vec::new();
    for target in target_names() {
        let name = published_name(version, target);
        let path = assets.0.join(&name);
        let listed = sums
            .iter()
            .find(|(_, listed)| *listed == name)
            .ok_or_else(|| format!("{name} is not in the release's SHA256SUMS"))?;
        if sha256(&read(&path)?) != listed.0 {
            return Err(format!("{name}: not the bytes SHA256SUMS lists"));
        }
        verify_attestation(
            &path,
            &repository,
            &assets.0.join("attestation.sigstore.json"),
            &signer(&repository),
        )?;
        archives.push(path);
    }

    let out = TempDir::new("sidevoice-connector-npm-packages")?;
    let archives: Vec<&Path> = archives.iter().map(PathBuf::as_path).collect();
    let packages = build(&archives, &out.0)?;
    let names: Vec<&str> = packages.iter().map(|packed| packed.name.as_str()).collect();
    if names.last() != Some(&LAUNCHER) || packages.iter().any(|packed| packed.version != version) {
        return Err(format!("unexpected packages: {names:?}"));
    }
    check_trusted_publishing(&names)?;

    let mut staged = None;
    for packed in &packages {
        let spec = format!("{}@{version}", packed.name);
        // A re-run carries on: a version already published with these very bytes is left as it is.
        match published_shasum(&packed.name, version)? {
            Some(shasum) if shasum == packed.shasum => {
                println!("{spec} is already published with these bytes");
                continue;
            }
            Some(shasum) => {
                return Err(format!(
                    "{spec} is already published with other bytes (shasum {shasum}, this build {}); npm versions are \
                     immutable: release a new version",
                    packed.shasum
                ))
            }
            None => {}
        }
        let tarball = path_str(&packed.tarball)?;
        let tag = if packed.name == LAUNCHER {
            dist_tag
        } else {
            PLATFORM_DIST_TAG
        };
        let common = ["--access", "public", "--provenance", "--tag", tag, "--json"];
        if packed.name == LAUNCHER {
            // Staged: a maintainer approves it on npmjs.com with 2FA. Until then no platform version reaches anyone.
            let mut args = vec!["stage", "publish", tarball];
            args.extend(common);
            let report =
                npm("npm-cli.js", &args, None).map_err(|error| publish_failed(&spec, &error))?;
            staged = Some(stage_id(&report).unwrap_or_else(|| "(see npm stage list)".into()));
        } else {
            let mut args = vec!["publish", tarball];
            args.extend(common);
            npm("npm-cli.js", &args, None).map_err(|error| publish_failed(&spec, &error))?;
            println!("published {spec} (dist-tag {tag})");
        }
    }
    match staged {
        Some(id) => println!(
            "{LAUNCHER}@{version} is STAGED (stage id {id}, dist-tag {dist_tag}), awaiting a maintainer's approval: \
             npmjs.com → {LAUNCHER} → staged versions, or `npm stage approve {id}` (2FA). Until it is approved \
             `npx {LAUNCHER}` installs the previous version; the platform packages are published."
        ),
        None => println!("{LAUNCHER}@{version} is already published; nothing to approve"),
    }
    Ok(())
}

fn publish_failed(spec: &str, error: &str) -> String {
    format!(
        "{spec}: npm refused it. Publishing uses trusted publishing only (repository {REPOSITORY}, workflow \
         {}); if the version is already staged, approve or reject it on npmjs.com. {error}",
        calling_workflow()
    )
}

/// The stage id in `npm stage publish --json`'s report.
fn stage_id(report: &str) -> Option<String> {
    fn find(value: &Value) -> Option<String> {
        match value {
            Value::Object(map) => map
                .get("stageId")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| map.values().find_map(find)),
            Value::Array(items) => items.iter().find_map(find),
            _ => None,
        }
    }
    find(&serde_json::from_str(report).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_target_has_its_npm_platform() {
        assert_eq!(
            platform_package("linux-x86_64").unwrap(),
            "@sidevoice/sidevoice-linux-x64"
        );
        assert_eq!(
            platform_package("linux-aarch64").unwrap(),
            "@sidevoice/sidevoice-linux-arm64"
        );
        assert_eq!(
            platform_package("macos-aarch64").unwrap(),
            "@sidevoice/sidevoice-darwin-arm64"
        );
        assert!(platform_package("windows-x86_64").is_err());
    }

    #[test]
    fn a_platform_package_installs_only_on_its_platform() {
        let linux = platform_manifest("linux-aarch64", "0.7.0").unwrap();
        assert_eq!(linux["os"], json!(["linux"]));
        assert_eq!(linux["cpu"], json!(["arm64"]));
        assert_eq!(linux["libc"], json!(["glibc"]));
        assert_eq!(linux["version"], "0.7.0");
        let mac = platform_manifest("macos-aarch64", "0.7.0").unwrap();
        assert_eq!(mac["os"], json!(["darwin"]));
        assert!(mac.get("libc").is_none());
        // Only the launcher exposes a command, and only it is ever `latest` or `next`.
        assert!(linux.get("bin").is_none() && mac.get("bin").is_none());
        assert!(!["latest", "next"].contains(&PLATFORM_DIST_TAG));
        assert_eq!(
            mac["repository"]["url"],
            "git+https://github.com/sidevoice/sidevoice-connector.git"
        );
    }

    #[test]
    fn the_launcher_pins_every_platform_package_at_exactly_its_version() {
        let manifest = launcher_manifest("0.7.0-rc.1").unwrap();
        assert_eq!(manifest["name"], "sidevoice");
        assert_eq!(manifest["bin"], json!({"sidevoice": "bin/sidevoice.js"}));
        assert_eq!(
            manifest["optionalDependencies"],
            json!({"@sidevoice/sidevoice-darwin-arm64": "0.7.0-rc.1",
                   "@sidevoice/sidevoice-linux-arm64": "0.7.0-rc.1",
                   "@sidevoice/sidevoice-linux-x64": "0.7.0-rc.1"})
        );
        assert!(manifest.get("dependencies").is_none());
    }

    #[test]
    fn the_launcher_script_names_the_platform_packages_as_xtask_does() {
        assert!(LAUNCHER_JS.starts_with("#!/usr/bin/env node\n"));
        assert!(LAUNCHER_JS.contains(&format!("const PREFIX = \"{PLATFORM_PREFIX}\";")));
    }

    #[test]
    fn tarballs_are_named_as_npm_pack_names_them() {
        assert_eq!(tarball_name("sidevoice", "0.7.0"), "sidevoice-0.7.0.tgz");
        assert_eq!(
            tarball_name("@sidevoice/sidevoice-linux-x64", "0.7.0-rc.1"),
            "sidevoice-sidevoice-linux-x64-0.7.0-rc.1.tgz"
        );
    }

    #[test]
    fn a_release_goes_to_latest_and_a_candidate_to_next() {
        assert_eq!(dist_tag("0.7.0").unwrap(), "latest");
        assert_eq!(dist_tag("1.10.3").unwrap(), "latest");
        assert_eq!(dist_tag("0.7.0-rc.1").unwrap(), "next");
        assert_eq!(dist_tag("0.7.0-rc.12").unwrap(), "next");
        for refused in [
            "nightly",
            "0.7",
            "0.7.0-beta.1",
            "0.7.0-rc",
            "0.7.0-rc.01",
            "01.7.0",
            "0.7.0-rc.1.2",
            "",
        ] {
            assert!(dist_tag(refused).is_err(), "{refused}");
        }
    }

    #[test]
    fn every_package_carries_its_metadata_and_readme() {
        let launcher = launcher_manifest("0.7.0").unwrap();
        let platform = platform_manifest("linux-x86_64", "0.7.0").unwrap();
        for manifest in [&launcher, &platform] {
            assert_eq!(manifest["license"], "Apache-2.0");
            for field in ["description", "homepage"] {
                assert!(manifest[field]
                    .as_str()
                    .is_some_and(|text| !text.is_empty()));
            }
            assert!(manifest["bugs"]["url"].is_string());
        }
        assert!(launcher["keywords"]
            .as_array()
            .is_some_and(|words| !words.is_empty()));
        assert!(platform["description"]
            .as_str()
            .unwrap()
            .contains("Platform binary for sidevoice (linux/x64)"));

        let readme = platform_readme("macos-aarch64", "0.7.0").unwrap();
        assert!(
            readme.contains("Platform binary for [`sidevoice`]"),
            "{readme}"
        );
        assert!(readme.contains("(darwin/arm64), version 0.7.0"), "{readme}");
        assert!(readme.contains("Do not install directly"), "{readme}");
        let readme = launcher_readme("0.7.0").unwrap();
        for wanted in [
            "npx sidevoice install",
            "macOS on Apple silicon (`darwin-arm64`)",
            "Linux on x64 (`linux-x64`) with glibc 2.28 or newer",
            "Linux on arm64 (`linux-arm64`)",
            "(0.7.0)",
            "https://github.com/sidevoice/sidevoice-connector",
        ] {
            assert!(readme.contains(wanted), "{wanted}: {readme}");
        }
    }

    #[test]
    fn the_stage_id_is_read_from_npm_s_report() {
        assert_eq!(
            stage_id(r#"{"sidevoice": {"id": "sidevoice@0.7.0", "stageId": "abc-123"}}"#)
                .as_deref(),
            Some("abc-123")
        );
        assert_eq!(stage_id("not json"), None);
    }

    #[test]
    fn the_workflow_npm_sees_is_the_calling_one() {
        // GITHUB_WORKFLOW_REF is not set in tests run outside Actions, or names this very run's workflow in CI.
        let workflow = calling_workflow();
        assert!(
            !workflow.contains('@') && !workflow.contains('/'),
            "{workflow}"
        );
    }
}
