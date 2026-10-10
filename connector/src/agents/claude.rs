use super::*;

impl HostAgents {
    async fn claude_entry_state(
        &self,
        binary: Option<&str>,
        cancel: &Cancellation,
        deadline: Instant,
    ) -> std::result::Result<String, Failure> {
        match fs::symlink_metadata(&self.profile.claude) {
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok("absent".into()),
            Err(_) => return Ok("invalid".into()),
            Ok(_) => {}
        }
        let Some(binary) = binary else {
            return Ok("unknown".into());
        };
        let output = match run_command(
            &self.profile,
            Some(&self.profile.claude),
            binary,
            &["mcp".into(), "get".into(), "sidevoice".into()],
            cancel,
            deadline,
        )
        .await
        {
            Ok(output) => output,
            Err(CommandFailure::Cancelled) => return Err(Failure::Cancelled),
            Err(_) => return Ok("unknown".into()),
        };
        if !output.status.success() {
            let detail = format!("{}\n{}", output.stdout, output.stderr);
            if (output.status.code() == Some(1) && detail.trim().is_empty())
                || looks_absent(&detail)
            {
                return Ok("absent".into());
            }
            return Ok("unknown".into());
        }
        let detail = format!("{}\n{}", output.stdout, output.stderr);
        let field = |name: &str| -> Option<String> {
            detail.lines().find_map(|line| {
                let (key, value) = line.trim().split_once(':')?;
                key.eq_ignore_ascii_case(name)
                    .then(|| value.trim().to_owned())
            })
        };
        let Some(command) = field("Command").filter(|value| !value.is_empty()) else {
            return Ok(if looks_absent(&detail) {
                "absent"
            } else {
                "unknown"
            }
            .into());
        };
        let args = field("Args")
            .map(|value| {
                value
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let user_scope =
            field("Scope").is_some_and(|value| value.to_ascii_lowercase().contains("user"));
        if !user_scope {
            return Ok("foreign".into());
        }
        // The block under `Environment:`, one `KEY=value` per line, indented below the field.
        let env = detail
            .lines()
            .skip_while(|line| !line.trim().eq_ignore_ascii_case("Environment:"))
            .skip(1)
            .take_while(|line| line.starts_with("    ") && line.contains('='))
            .filter_map(|line| line.trim().split_once('='))
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect::<BTreeMap<_, _>>();
        if self.selected.owns(&command, &args) {
            Ok(if command == self.selected.command
                && args == self.selected.args
                && env == self.mcp_env()
            {
                "connected"
            } else {
                "owned-old"
            }
            .into())
        } else {
            Ok("foreign".into())
        }
    }

    pub(super) async fn claude_registration(
        &self,
        binary: Option<&str>,
        cancel: &Cancellation,
        deadline: Instant,
    ) -> std::result::Result<String, Failure> {
        let state = self.claude_entry_state(binary, cancel, deadline).await?;
        Ok(match state.as_str() {
            "absent" => "not-connected".into(),
            "owned-old" => "outdated".into(),
            _ => state,
        })
    }

    pub(super) async fn claude_connect(
        &self,
        binary: Option<&str>,
        cancel: &Cancellation,
        deadline: Instant,
    ) -> std::result::Result<(), Failure> {
        let binary =
            binary.ok_or_else(|| agent_failure("agents.manual-required", AgentId::Claude))?;
        let state = self
            .claude_entry_state(Some(binary), cancel, deadline)
            .await?;
        if state == "connected" {
            return Ok(());
        }
        if state == "foreign" {
            return Err(agent_failure("agents.foreign", AgentId::Claude));
        }
        if state == "invalid" {
            return Err(agent_failure("agents.invalid", AgentId::Claude));
        }
        if state == "unknown" {
            return Err(agent_failure(
                "agents.registration-unknown",
                AgentId::Claude,
            ));
        }
        check_live(cancel, deadline)?;
        if state == "absent" || state == "owned-old" {
            // A second probe just before mutation closes the gap with an external CLI writer.
            let again = self
                .claude_entry_state(Some(binary), cancel, deadline)
                .await?;
            if again == "foreign" {
                return Err(agent_failure("agents.foreign", AgentId::Claude));
            }
            if again == "invalid" {
                return Err(agent_failure("agents.invalid", AgentId::Claude));
            }
            if again == "unknown" {
                return Err(agent_failure(
                    "agents.registration-unknown",
                    AgentId::Claude,
                ));
            }
            if again == "connected" {
                return Ok(());
            }
            if again == "owned-old" {
                self.run_cli_required(
                    binary,
                    &["mcp", "remove", "--scope", "user", "sidevoice"],
                    cancel,
                    deadline,
                    AgentId::Claude,
                )
                .await?;
            }
        }
        let args = self.claude_add_args();
        self.run_cli_required(binary, &args, cancel, deadline, AgentId::Claude)
            .await
    }

    /// `mcp add` for this installation: its command, and the environment it runs with. `-e` takes every value up
    /// to the next option, so the server's name comes before it.
    pub(super) fn claude_add_args(&self) -> Vec<String> {
        let mut args = vec![
            "mcp".to_owned(),
            "add".into(),
            "--scope".into(),
            "user".into(),
            "sidevoice".into(),
        ];
        for (key, value) in self.mcp_env() {
            args.push("-e".into());
            args.push(format!("{key}={value}"));
        }
        args.push("--".into());
        args.push(self.selected.command.clone());
        args.extend(self.selected.args.clone());
        args
    }

    pub(super) async fn claude_disconnect(
        &self,
        binary: Option<&str>,
        cancel: &Cancellation,
        deadline: Instant,
    ) -> std::result::Result<(), Failure> {
        let binary =
            binary.ok_or_else(|| agent_failure("agents.registration-unknown", AgentId::Claude))?;
        let state = self
            .claude_registration(Some(binary), cancel, deadline)
            .await?;
        match state.as_str() {
            "not-connected" => Ok(()),
            // Ours, current or not (an older release, another environment): removed.
            "connected" | "outdated" => {
                self.run_cli_required(
                    binary,
                    &["mcp", "remove", "--scope", "user", "sidevoice"],
                    cancel,
                    deadline,
                    AgentId::Claude,
                )
                .await
            }
            "foreign" => Err(agent_failure("agents.foreign", AgentId::Claude)),
            "invalid" => Err(agent_failure("agents.invalid", AgentId::Claude)),
            _ => Err(agent_failure(
                "agents.registration-unknown",
                AgentId::Claude,
            )),
        }
    }
}
