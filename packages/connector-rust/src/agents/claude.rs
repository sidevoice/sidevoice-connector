use super::*;

impl HostAgents {
    async fn claude_entry_state(
        &self,
        binary: Option<&str>,
        cancel: &Cancellation,
        deadline: Instant,
    ) -> std::result::Result<String, Failure> {
        let Some(binary) = binary else { return Ok("unknown".into()) };
        let output = match run_command(
            &self.profile,
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
                key.eq_ignore_ascii_case(name).then(|| value.trim().to_owned())
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
            .map(|value| value.split_whitespace().map(str::to_owned).collect::<Vec<_>>())
            .unwrap_or_default();
        let user_scope = field("Scope")
            .is_some_and(|value| value.to_ascii_lowercase().contains("user"));
        if !user_scope {
            return Ok("foreign".into());
        }
        if self.selected.owns(AgentId::Claude, &command, &args, None) {
            Ok(if command == self.selected.command && args == self.selected.args {
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
        Ok(if state == "absent" || state == "owned-old" {
            "not-connected".into()
        } else {
            state
        })
    }

    pub(super) async fn claude_connect(
        &self,
        binary: Option<&str>,
        cancel: &Cancellation,
        deadline: Instant,
    ) -> std::result::Result<(), Failure> {
        let binary = binary.ok_or_else(|| agent_failure("agents.manual-required", AgentId::Claude))?;
        let state = self.claude_entry_state(Some(binary), cancel, deadline).await?;
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
            return Err(agent_failure("agents.registration-unknown", AgentId::Claude));
        }
        check_live(cancel, deadline)?;
        if state == "absent" || state == "owned-old" {
            // A second probe just before mutation closes the gap with an external CLI writer.
            let again = self.claude_entry_state(Some(binary), cancel, deadline).await?;
            if again == "foreign" {
                return Err(agent_failure("agents.foreign", AgentId::Claude));
            }
            if again == "invalid" {
                return Err(agent_failure("agents.invalid", AgentId::Claude));
            }
            if again == "unknown" {
                return Err(agent_failure("agents.registration-unknown", AgentId::Claude));
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
        let mut args = vec!["mcp", "add", "--scope", "user", "sidevoice", "--"]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        args.push(self.selected.command.clone());
        args.extend(self.selected.args.clone());
        self.run_cli_required(binary, &args, cancel, deadline, AgentId::Claude)
            .await
    }

    pub(super) async fn claude_disconnect(
        &self,
        binary: Option<&str>,
        cancel: &Cancellation,
        deadline: Instant,
    ) -> std::result::Result<(), Failure> {
        let binary = binary.ok_or_else(|| agent_failure("agents.registration-unknown", AgentId::Claude))?;
        let state = self.claude_registration(Some(binary), cancel, deadline).await?;
        match state.as_str() {
            "not-connected" => Ok(()),
            "connected" => {
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
            _ => Err(agent_failure("agents.registration-unknown", AgentId::Claude)),
        }
    }


}
