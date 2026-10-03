use super::*;

impl HostAgents {
    pub(super) async fn codex_registration(
        &self,
        binary: Option<&str>,
        cancel: &Cancellation,
        deadline: Instant,
    ) -> std::result::Result<String, Failure> {
        let guard = self.codex_file_guard()?;
        if guard == "foreign" || guard == "invalid" {
            return Ok(guard);
        }
        let Some(binary) = binary else {
            return Ok("unknown".into());
        };
        let output = match run_command(
            &self.profile,
            binary,
            &[
                "mcp".into(),
                "get".into(),
                "sidevoice".into(),
                "--json".into(),
            ],
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
            let detail = format!("{}\n{}", output.stderr, output.stdout);
            if guard == "not-connected" && looks_absent(&detail) {
                return Ok("not-connected".into());
            }
            return Ok("unknown".into());
        }
        let entry: Value = match serde_json::from_str(output.stdout.trim()) {
            Ok(value) => value,
            Err(_) => return Ok("unknown".into()),
        };
        let transport = entry.get("transport");
        let command = transport
            .and_then(|value| value.get("command"))
            .and_then(Value::as_str);
        let args = transport
            .and_then(|value| value.get("args"))
            .and_then(Value::as_array)
            .and_then(|values| {
                values
                    .iter()
                    .map(|value| value.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
            });
        if entry.get("name").and_then(Value::as_str) != Some("sidevoice")
            || transport
                .and_then(|value| value.get("type"))
                .and_then(Value::as_str)
                != Some("stdio")
            || command.is_none()
            || args.is_none()
        {
            return Ok("invalid".into());
        }
        let command = command.unwrap_or_default();
        let args = args.unwrap_or_default();
        if !self.selected.owns(
            AgentId::Codex,
            command,
            &args,
            transport.and_then(|value| value.get("env")),
        ) {
            return Ok("foreign".into());
        }
        let enabled = entry.get("enabled").and_then(Value::as_bool) != Some(false);
        Ok(
            if enabled && command == self.selected.command && args == self.selected.args {
                "connected"
            } else {
                "not-connected"
            }
            .into(),
        )
    }

    fn codex_file_guard(&self) -> std::result::Result<String, Failure> {
        let file = self.profile.codex.join("config.toml");
        if !file.exists() {
            return Ok("not-connected".into());
        }
        let text = match read_profile_file(&file, &self.profile.root, 1 << 20) {
            Ok(text) => text,
            Err(_) => return Ok("invalid".into()),
        };
        let config: toml::Value = match toml::from_str(&text) {
            Ok(value) => value,
            Err(_) => return Ok("invalid".into()),
        };
        let Some(servers) = config.get("mcp_servers") else {
            return Ok("not-connected".into());
        };
        let Some(servers) = servers.as_table() else {
            return Ok("invalid".into());
        };
        let Some(entry) = servers.get("sidevoice") else {
            return Ok("not-connected".into());
        };
        let Some(entry) = entry.as_table() else {
            return Ok("invalid".into());
        };
        let Some(command) = entry.get("command").and_then(toml::Value::as_str) else {
            return Ok("invalid".into());
        };
        let Some(args) = entry.get("args").and_then(toml::Value::as_array) else {
            return Ok("invalid".into());
        };
        let Some(args) = args
            .iter()
            .map(|value| value.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
        else {
            return Ok("invalid".into());
        };
        let env = entry.get("env").and_then(toml_to_json);
        Ok(if self
            .selected
            .owns(AgentId::Codex, command, &args, env.as_ref())
        {
            "ours"
        } else {
            "foreign"
        }
        .into())
    }

    pub(super) async fn codex_connect(
        &self,
        binary: Option<&str>,
        cancel: &Cancellation,
        deadline: Instant,
    ) -> std::result::Result<(), Failure> {
        let binary =
            binary.ok_or_else(|| agent_failure("agents.manual-required", AgentId::Codex))?;
        let state = self
            .codex_registration(Some(binary), cancel, deadline)
            .await?;
        if state == "connected" {
            return Ok(());
        }
        if state == "foreign" {
            return Err(agent_failure("agents.foreign", AgentId::Codex));
        }
        if state == "invalid" {
            return Err(agent_failure("agents.invalid", AgentId::Codex));
        }
        if state == "unknown" {
            return Err(agent_failure("agents.registration-unknown", AgentId::Codex));
        }
        let again = self
            .codex_registration(Some(binary), cancel, deadline)
            .await?;
        if again == "foreign" {
            return Err(agent_failure("agents.foreign", AgentId::Codex));
        }
        if again == "invalid" {
            return Err(agent_failure("agents.invalid", AgentId::Codex));
        }
        if again == "unknown" {
            return Err(agent_failure("agents.registration-unknown", AgentId::Codex));
        }
        if again == "connected" {
            return Ok(());
        }
        if self.codex_file_guard()? == "ours" {
            self.run_cli_required(
                binary,
                &["mcp", "remove", "sidevoice"],
                cancel,
                deadline,
                AgentId::Codex,
            )
            .await?;
        }
        let mut args = vec!["mcp", "add", "sidevoice", "--"]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        args.push(self.selected.command.clone());
        args.extend(self.selected.args.clone());
        self.run_cli_required(binary, &args, cancel, deadline, AgentId::Codex)
            .await
    }

    pub(super) async fn codex_disconnect(
        &self,
        binary: Option<&str>,
        cancel: &Cancellation,
        deadline: Instant,
    ) -> std::result::Result<(), Failure> {
        let binary =
            binary.ok_or_else(|| agent_failure("agents.registration-unknown", AgentId::Codex))?;
        let state = self
            .codex_registration(Some(binary), cancel, deadline)
            .await?;
        match state.as_str() {
            "not-connected" => Ok(()),
            "connected" => {
                self.run_cli_required(
                    binary,
                    &["mcp", "remove", "sidevoice"],
                    cancel,
                    deadline,
                    AgentId::Codex,
                )
                .await
            }
            "foreign" => Err(agent_failure("agents.foreign", AgentId::Codex)),
            "invalid" => Err(agent_failure("agents.invalid", AgentId::Codex)),
            _ => Err(agent_failure("agents.registration-unknown", AgentId::Codex)),
        }
    }
}
