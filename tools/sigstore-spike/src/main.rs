use sidevoice_sigstore_spike::{core_policy, npm_fixture_policy, read_sidecar, verify_file};
use sigstore_trust_root::{TrustedRoot, TufConfig};
use std::{env, path::PathBuf, time::Duration};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("tuf-future") {
        if args.len() != 2 { return Err("usage: sidevoice-sigstore-spike tuf-future CACHE_DIR".into()); }
        let time: jiff::Timestamp = "2100-01-01T00:00:00Z".parse().map_err(|e| format!("future time: {e}"))?;
        let config = TufConfig::production().with_cache_dir(PathBuf::from(&args[1])).offline();
        match TrustedRoot::from_tuf_at(config, time).await {
            Ok(_) => return Err("TUF accepted unchanged signed metadata after expiry".into()),
            Err(error) => { println!("future-time TUF refusal: {error}"); return Ok(()); }
        }
    }
    if args.len() < 5 || args.len() > 7 {
        return Err("usage: sidevoice-sigstore-spike ARTIFACT SIDECAR nightly|release|npm CACHE_DIR online|offline [EXPECTED_SHA256 [EXPECTED_SIZE]]".into());
    }
    let cache = PathBuf::from(&args[3]);
    let client = sigstore_tuf::reqwest::Client::builder().timeout(Duration::from_secs(20))
        .build().map_err(|e| format!("HTTP client: {e}"))?;
    let mut config = TufConfig::production().with_cache_dir(cache).with_http_client(client);
    match args[4].as_str() {
        "online" => {},
        "offline" => config = config.offline(),
        _ => return Err("TUF mode must be online or offline".into()),
    }
    let root = TrustedRoot::from_tuf(config).await.map_err(|e| format!("public-good TUF: {e}"))?;
    let sidecar = read_sidecar(&PathBuf::from(&args[1]))?;
    let channel = match args[2].as_str() {
        "nightly" | "release" => Some(args[2].as_str()),
        "npm" => None,
        _ => return Err("unsupported channel".into()),
    };
    let policy = if channel.is_some() { core_policy() } else { npm_fixture_policy() };
    let size = args.get(6).map(|value| value.parse::<u64>().map_err(|e| format!("expected size: {e}"))).transpose()?;
    let (digest, byte_count) = verify_file(&PathBuf::from(&args[0]), &sidecar, &root, &policy,
        channel, args.get(5).map(String::as_str), size)?;
    println!("verified channel={} sha256={digest} bytes={byte_count} certificate=true sct=true tlog=true identity=true", args[2]);
    Ok(())
}
