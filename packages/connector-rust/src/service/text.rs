//! The English text of every key the service commands answer with. The CLI is English only (#66, decision 8):
//! `--json` carries the stable key, and this text as its `message`, so a client can show its own translation.

use serde_json::Value;

fn template(key: &str) -> Option<&'static str> {
    Some(match key {
        "service.failed" => "The Sidevoice service command failed.",
        "service.busy" => "Another Sidevoice install or service command is under way ({detail}); nothing was changed. Try again when it has finished.",
        "service.no-installation" => "Sidevoice is not installed on this computer: run `sidevoice install` first.",
        "service.no-manager" => "This computer has no service manager for your user (launchd, or systemd --user), so Sidevoice cannot start at login here; it starts on demand when a conversation needs it.",
        "service.unsafe-value" => "A value for the service definition ({what}) contains a control character; nothing was written.",
        "service.definition-unsafe" => "A Sidevoice service definition or its directory is unsafe ({detail}); nothing was changed.",
        "service.not-loaded" => "The Sidevoice service is installed but its service manager did not start it ({detail}). Start it with `sidevoice service start`, or install it again with `sidevoice service install`.",
        "service.start-limit" => "The Sidevoice service failed too many times in a row and its service manager stopped trying ({detail}). Retry with `sidevoice service restart`.",
        "service.executable-missing" => "The Sidevoice service points at a program that is not there any more ({detail}). Install Sidevoice again.",
        "service.permission-denied" => "The Sidevoice service cannot run its program: permission denied ({detail}).",
        "service.unload-failed" => "The service manager did not unload the Sidevoice service ({detail}); nothing was deleted.",
        "service.killed" => "Processes that did not stop in time were killed: {pids}.",
        "service.linger-reason" => "Linux runs your services only while you are logged in. To keep Sidevoice running without a session (after a reboot, over SSH), the machine's owner can enable lingering for your user. It is a system setting, so Sidevoice does not change it:",
        "service.state.absent" => "Sidevoice is not installed on this computer.",
        "service.state.not-installed" => "Sidevoice is installed but does not start at login (no service). Enable it with `sidevoice service install`.",
        "service.state.stopped-by-person" => "Sidevoice is stopped until you start it (`sidevoice service start`) or log in again.",
        "service.state.starting" => "The Sidevoice core is starting.",
        "service.state.running" => "Sidevoice is running ({service}).",
        "service.state.backoff" => "The Sidevoice core failed and its service manager is starting it again.",
        "service.state.failed" => "The Sidevoice core failed and is not started again by itself: `sidevoice service restart` tries again.",
        "service.state.service-failed" => "The Sidevoice service is not running.",
        "not-loaded" => "The service manager has not loaded the core's job.",
        "start-limit" => "The core's job failed too many times in a row and its service manager stopped trying.",
        "executable-missing" => "The core's job points at a program that is not there any more.",
        "permission-denied" => "The core's job cannot run its program: permission denied.",
        "node.stopped" => "Sidevoice is stopped on this computer: start it from the app or with `sidevoice service start`.",
        "connector.not-started" => "The Sidevoice connector did not start.",
        "connector.service-down" => "The Sidevoice connector is not answering ({detail}); its service manager runs it. See `sidevoice service status`, or retry with `sidevoice service restart`.",
        "connector.release-changed" => "The selected Sidevoice release changed; restart this conversation's MCP server.",
        "launch.exited" => "The core exited ({detail}).",
        "launch.missing-executable" => "The core's program is not where it was installed ({detail}).",
        "launch.permission" => "The core's program cannot be run: permission denied ({detail}).",
        "ready.timeout" => "The core did not become ready in time.",
        "hang" => "The core stopped answering.",
        "bind.core-running" => "Another Sidevoice core is already running for this data directory.",
        "start.failed" => "The core could not start ({detail}).",
        _ => return None,
    })
}

/// `key`'s text with `{name}` replaced by `params[name]`; an unknown key is its own text.
pub fn message(key: &str, params: &Value) -> String {
    let Some(template) = template(key) else {
        return key.to_owned();
    };
    let mut rendered = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        rendered.push_str(&rest[..open]);
        let tail = &rest[open + 1..];
        let Some(close) = tail.find('}') else {
            rendered.push_str(&rest[open..]);
            return rendered;
        };
        match params.get(&tail[..close]) {
            Some(Value::String(text)) => rendered.push_str(text),
            Some(Value::Null) | None => rendered.push('?'),
            Some(other) => rendered.push_str(&other.to_string()),
        }
        rest = &tail[close + 1..];
    }
    rendered.push_str(rest);
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn templates_take_their_parameters_and_unknown_keys_stay_keys() {
        assert_eq!(
            message("launch.exited", &json!({"detail": 3})),
            "The core exited (3)."
        );
        assert_eq!(
            message("launch.exited", &Value::Null),
            "The core exited (?)."
        );
        assert_eq!(
            message("service.state.running", &json!({"service": "systemd"})),
            "Sidevoice is running (systemd)."
        );
        assert_eq!(message("no.such-key", &Value::Null), "no.such-key");
    }
}
