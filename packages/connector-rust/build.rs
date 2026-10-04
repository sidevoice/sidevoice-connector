use std::env;

fn build_value(name: &str, fallback: &str) -> String {
    match env::var(name) {
        Ok(value) if !value.is_empty() && !value.contains('\n') && !value.contains('\r') => value,
        Ok(_) => panic!("{name} is malformed"),
        Err(_) => fallback.to_owned(),
    }
}

fn main() {
    for name in [
        "SIDEVOICE_CONNECTOR_BUILD_SHA",
        "SIDEVOICE_CONNECTOR_TARGET",
        "SIDEVOICE_CONNECTOR_VERSION",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    let source = build_value("SIDEVOICE_CONNECTOR_BUILD_SHA", "development");
    let target = build_value("SIDEVOICE_CONNECTOR_TARGET", "development");
    let version = build_value("SIDEVOICE_CONNECTOR_VERSION", env!("CARGO_PKG_VERSION"));
    println!("cargo:rustc-env=SIDEVOICE_CONNECTOR_BUILD_SHA={source}");
    println!("cargo:rustc-env=SIDEVOICE_CONNECTOR_TARGET={target}");
    println!("cargo:rustc-env=SIDEVOICE_CONNECTOR_VERSION={version}");
}
