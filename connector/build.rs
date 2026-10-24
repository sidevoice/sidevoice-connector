use std::env;

fn build_value(name: &str, fallback: &str) -> String {
    match env::var(name) {
        Ok(value) if !value.is_empty() && !value.contains('\n') && !value.contains('\r') => value,
        Ok(_) => panic!("{name} is malformed"),
        Err(_) => fallback.to_owned(),
    }
}

/// The manifest Windows reads from the binaries: they handle paths longer than `MAX_PATH` (260 characters), which a
/// release tree under `%LOCALAPPDATA%` can reach. Takes effect where the system allows it (`LongPathsEnabled`, on by
/// default on current Windows 11); Rust's own file functions already use the `\\?\` form that needs no manifest.
fn embed_windows_manifest() {
    use embed_manifest::manifest::Setting;
    embed_manifest::embed_manifest(
        embed_manifest::new_manifest("Sidevoice.Connector").long_path_aware(Setting::Enabled),
    )
    .expect("cannot embed the Windows manifest");
    println!("cargo:rerun-if-changed=build.rs");
}

/// The version resource of the windowless launcher (`sidevoice-background`) alone: Task Manager shows its description,
/// "Sidevoice Background", for the process. The connector, a console program a person runs, gets none.
fn embed_background_resource(version: &str) {
    let parsed = semver::Version::parse(version).expect("connector version must be semantic");
    let numbers = format!("{},{},{},0", parsed.major, parsed.minor, parsed.patch);
    let resource = std::fs::read_to_string("background.rc")
        .expect("cannot read the launcher's version resource")
        .replace("@VERSION_NUMBERS@", &numbers)
        .replace("@VERSION@", version);
    let path = std::path::PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set"))
        .join("background.rc");
    std::fs::write(&path, resource).expect("cannot write the launcher's version resource");
    embed_resource::compile_for(&path, ["sidevoice-background"], embed_resource::NONE)
        .manifest_required()
        .expect("cannot embed the launcher's version resource");
    println!("cargo:rerun-if-changed=background.rc");
}

fn main() {
    let version = build_value("SIDEVOICE_CONNECTOR_VERSION", env!("CARGO_PKG_VERSION"));
    if env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows") {
        embed_windows_manifest();
        embed_background_resource(&version);
    }
    for name in [
        "SIDEVOICE_CONNECTOR_BUILD_SHA",
        "SIDEVOICE_CONNECTOR_TARGET",
        "SIDEVOICE_CONNECTOR_VERSION",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    let source = build_value("SIDEVOICE_CONNECTOR_BUILD_SHA", "development");
    let target = build_value("SIDEVOICE_CONNECTOR_TARGET", "development");
    println!("cargo:rustc-env=SIDEVOICE_CONNECTOR_BUILD_SHA={source}");
    println!("cargo:rustc-env=SIDEVOICE_CONNECTOR_TARGET={target}");
    println!("cargo:rustc-env=SIDEVOICE_CONNECTOR_VERSION={version}");
}
