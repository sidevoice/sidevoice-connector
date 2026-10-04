//! Public native CLI. The proof CLI remains explicitly isolated behind --profile-root.
use crate::{agents, pairing, payload, proof::Profile};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    net::UnixStream,
    time::{timeout, Duration},
};

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|value| value == name)
}
fn message(key: &str, params: &Value) -> String {
    agents::message(key, params)
}

/// None means an explicitly retained proof/runtime command should use the existing parser.
pub async fn run(args: &[String]) -> Option<i32> {
    let command = args.first().map(String::as_str).unwrap_or("");
    if matches!(command, "runtime-identity" | "codex" | "--installed") {
        return None;
    }
    let json_output =
        flag(args, "--json") || command == "metadata" || command.starts_with("--sidevoice-");
    let result = dispatch(command, &args[1.min(args.len())..]).await;
    Some(match result {
        Ok(answer) => {
            if let Some(text) = answer.get("_text").and_then(Value::as_str) {
                if json_output {
                    println!("{}", answer.get("result").unwrap_or(&Value::Null));
                } else {
                    println!("{text}");
                }
            } else if answer != Value::Null {
                if json_output {
                    println!("{answer}");
                } else if let Some(text) = answer.get("text").and_then(Value::as_str) {
                    println!("{text}");
                } else if let Some(done) = answer.get("done").and_then(Value::as_array) {
                    for line in done {
                        if let Some(line) = line.as_str() {
                            println!("{line}");
                        }
                    }
                    if let Some(next) = answer.get("next").and_then(Value::as_array) {
                        for line in next {
                            if let Some(line) = line.as_str() {
                                println!("{line}");
                            }
                        }
                    }
                } else {
                    println!("{answer}");
                }
            }
            let result = answer
                .get("result")
                .filter(|_| answer.get("_text").is_some())
                .unwrap_or(&answer);
            if result.get("ok") == Some(&Value::Bool(false)) || result.get("error").is_some() {
                1
            } else {
                0
            }
        }
        Err(error) => {
            if error
                .downcast_ref::<crate::release::ControlError>()
                .is_some()
            {
                let answer = crate::lifecycle::error_value(&error);
                if json_output {
                    println!("{answer}");
                } else {
                    eprintln!("{}", answer["error"]["message"].as_str().unwrap_or(""));
                }
                return Some(1);
            }
            let detail = error.to_string();
            let key = if detail.split_whitespace().count() == 1 && detail.contains('.') {
                detail.as_str()
            } else {
                match command {
                    "install" => "install.failed",
                    "rollback" => "rollback.failed",
                    "uninstall" => "uninstall.failed",
                    "pair-device" => "pair-device.failed",
                    "pair" | "link-room" => "pair.failed",
                    "service" => "service.failed",
                    "agents" => "agents.failed",
                    _ => "command.unknown",
                }
            };
            let rendered = message(key, &json!({"detail":detail,"command":command}));
            if json_output {
                println!(
                    "{}",
                    json!({"ok":false,"error":{"key":key,"message":rendered}})
                );
            } else {
                eprintln!("{rendered}");
            }
            if key == "command.unknown" {
                2
            } else {
                1
            }
        }
    })
}

async fn dispatch(command: &str, args: &[String]) -> Result<Value> {
    match command {
        "--version" => Ok(
            json!({"_text":env!("SIDEVOICE_CONNECTOR_VERSION"),"result":payload::version_metadata()}),
        ),
        "metadata" => Ok(payload::connector_metadata()),
        "--sidevoice-selected-command" => Ok(json!(crate::installed_service::stable_command(
            &crate::release::Paths::environment()?
        )?)),
        "--sidevoice-runtime-switching" => Ok(
            json!({"switching":crate::installed_service::runtime_switching(&crate::release::Paths::environment()?)?}),
        ),
        "--sidevoice-connector-management" => Ok(
            json!({"state":if crate::installed_service::defined(&crate::release::Paths::environment()?)? { "defined" } else { "absent" }}),
        ),
        "--sidevoice-ensure-core" => {
            crate::installed_service::ensure_core(&crate::release::Paths::environment()?).await?;
            Ok(json!({"ok":true}))
        }
        "--sidevoice-agent-scan" => {
            let answer = agents::run_cli(Profile::for_control_env()?, &[]).await?;
            if answer.get("error").is_some() {
                bail!("agents.failed");
            }
            Ok(Value::Null)
        }

        "mcp" => {
            crate::mcp::run(Profile::from_installed_env()?).await?;
            Ok(Value::Null)
        }
        "connector" => {
            let profile = Profile::from_installed_env()?;
            crate::daemon::run(profile, flag(args, "--service")).await?;
            Ok(Value::Null)
        }
        "agents" => {
            let result = agents::run_cli(Profile::for_control_env()?, args).await?;
            Ok(json!({"_text":agents::cli_text(&result,args),"result":result}))
        }
        "skill" => {
            let result = crate::skill::run_cli(&Profile::for_control_env()?, args)?;
            Ok(json!({"_text":crate::skill::cli_text(&result),"result":result}))
        }
        "pair" | "link-room" => {
            let positional: Vec<&str> = args
                .iter()
                .filter(|arg| !matches!(arg.as_str(), "--" | "--json"))
                .map(String::as_str)
                .collect();
            let profile = Profile::for_control_env()?;
            if command == "pair" {
                if positional.len() != 2 {
                    bail!("pair.arguments");
                }
                let result = pairing::run_pair(&profile, positional[0], positional[1]).await?;
                Ok(json!({"_text":pairing::cli_text(&profile,&result,false),"result":result}))
            } else {
                if positional.len() != 1 {
                    bail!("pair.arguments");
                }
                let result = pairing::link_room(&profile, positional[0]).await?;
                Ok(json!({"_text":pairing::cli_text(&profile,&result,true),"result":result}))
            }
        }
        "pair-device" => {
            let profile = Profile::for_control_env()?;
            crate::service::ensure_connector(&profile).await?;
            let answer = rpc(&profile, "pair_device", json!({})).await?;
            let result = pairing::device_result(&answer)?;
            Ok(json!({"_text":pairing::device_text(&answer)?,"result":result}))
        }
        "install" => {
            if flag(args, "--no-core") {
                bail!("install.native-core-required");
            }
            let cancel = Arc::new(AtomicBool::new(false));
            let signal = cancel.clone();
            let task = tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    signal.store(true, Ordering::SeqCst);
                }
            });
            let result = install(args, cancel).await;
            task.abort();
            result
        }
        "rollback" => crate::lifecycle::rollback(flag(args, "--progress=jsonl")).await,
        "uninstall" => crate::lifecycle::uninstall().await,
        "service" => {
            let action = match args.first().map(String::as_str) {
                Some("install") => crate::service::Action::Install,
                Some("uninstall") => crate::service::Action::Uninstall,
                Some("start") => crate::service::Action::Start,
                Some("stop") => crate::service::Action::Stop,
                Some("restart") => crate::service::Action::Restart,
                Some("status") => crate::service::Action::Status,
                _ => bail!("service.usage"),
            };
            crate::installed_service::run(&crate::release::Paths::environment()?, action).await
        }
        _ => bail!("command.unknown"),
    }
}

async fn install(args: &[String], cancel: Arc<AtomicBool>) -> Result<Value> {
    validate_install_args(args)?;
    let manifest = payload::MANIFEST;
    let archive = payload::core_archive()?;
    crate::lifecycle::install(
        crate::lifecycle::Payload {
            manifest,
            archive,
            core_version: payload::CORE_VERSION,
            channel: env!("SIDEVOICE_CHANNEL"),
            build_seq: env!("SIDEVOICE_BUILD_SEQ").parse()?,
        },
        crate::lifecycle::InstallOptions {
            service: flag(args, "--service"),
            apply_now: flag(args, "--apply-now"),
            progress: flag(args, "--progress=jsonl"),
            no_agents: flag(args, "--no-agents"),
            harnesses: args
                .windows(2)
                .filter(|pair| pair[0] == "--harness")
                .map(|pair| pair[1].clone())
                .collect(),
        },
        cancel,
    )
    .await
}

async fn rpc(profile: &Profile, method: &str, params: Value) -> Result<Value> {
    crate::proof::verify_socket(&profile.socket)?;
    timeout(Duration::from_secs(20), async {
        let mut stream = UnixStream::connect(&profile.socket).await?;
        let frame = json!({"id":1,"method":method,"params":params});
        stream.write_all(format!("{frame}\n").as_bytes()).await?;
        let mut reader = BufReader::new(stream);
        let line = crate::bounded_line(&mut reader, 1024 * 1024)
            .await?
            .context("missing IPC response")?;
        let response: Value = serde_json::from_str(&line)?;
        if response["id"] != 1 || response["ok"] != true {
            bail!("pair-device.failed");
        }
        Ok(response["result"].clone())
    })
    .await
    .context("pair-device.timeout")?
}

fn validate_install_args(args: &[String]) -> Result<()> {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" | "--progress=jsonl" | "--service" | "--apply-now" | "--no-agents" => {}
            "--harness" => {
                if !matches!(
                    args.next().map(String::as_str),
                    Some("claude" | "codex" | "cursor")
                ) {
                    bail!("install.usage");
                }
            }
            "--no-core" => bail!("install.native-core-required"),
            _ => bail!("install.usage"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_install_args;
    #[test]
    fn rejects_missing_or_unknown_harness_and_progress_without_side_effects() {
        for args in [
            vec!["--harness"],
            vec!["--harness", "foreign"],
            vec!["--progress=text"],
            vec!["--no-core"],
        ] {
            assert!(
                validate_install_args(&args.into_iter().map(String::from).collect::<Vec<_>>())
                    .is_err()
            );
        }
        assert!(validate_install_args(&[
            "--harness".into(),
            "codex".into(),
            "--json".into(),
            "--progress=jsonl".into()
        ])
        .is_ok());
    }
}
