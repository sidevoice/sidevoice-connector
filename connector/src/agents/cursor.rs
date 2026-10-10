use super::*;

impl HostAgents {
    pub(super) fn cursor_registration(&self) -> Result<String> {
        self.profile.validate_existing_private()?;
        let file = self.profile.cursor.join("mcp.json");
        let root = self.profile.agent_root(&self.profile.cursor)?;
        let Some((mut config, _mode, _target)) = read_cursor_config(&file, &root)? else {
            return Ok("not-connected".into());
        };
        let current = config
            .get_mut("mcpServers")
            .and_then(Value::as_object_mut)
            .and_then(|servers| servers.get("sidevoice"))
            .cloned();
        let Some(current) = current else {
            return Ok("not-connected".into());
        };
        let command = current.get("command").and_then(Value::as_str).unwrap_or("");
        let args = current
            .get("args")
            .and_then(Value::as_array)
            .and_then(|values| {
                values
                    .iter()
                    .map(|value| value.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
            })
            .unwrap_or_default();
        if !self.selected.owns(command, &args) {
            return Ok("foreign".into());
        }
        Ok(if command == self.selected.command
            && args == self.selected.args
            && cursor_env(&current) == self.mcp_env()
        {
            "connected"
        } else {
            "outdated"
        }
        .into())
    }

    pub(super) fn cursor_connect(&self) -> std::result::Result<(), Failure> {
        self.profile
            .validate_for_agent(&self.profile.cursor)
            .map_err(|_| agent_failure("agents.invalid", AgentId::Cursor))?;
        let file = self.profile.cursor.join("mcp.json");
        let root = self
            .profile
            .agent_root(&self.profile.cursor)
            .map_err(|_| agent_failure("agents.invalid", AgentId::Cursor))?;
        let mut config = match read_cursor_config(&file, &root) {
            Ok(Some((config, _, _))) => config,
            Ok(None) => json!({}),
            Err(_) => return Err(agent_failure("agents.invalid", AgentId::Cursor)),
        };
        if !config.is_object() {
            return Err(agent_failure("agents.invalid", AgentId::Cursor));
        }
        let servers = config
            .get("mcpServers")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if let Some(current) = servers.get("sidevoice") {
            let command = current.get("command").and_then(Value::as_str).unwrap_or("");
            let args = current
                .get("args")
                .and_then(Value::as_array)
                .and_then(|values| {
                    values
                        .iter()
                        .map(|value| value.as_str().map(str::to_owned))
                        .collect::<Option<Vec<_>>>()
                })
                .unwrap_or_default();
            if !self.selected.owns(command, &args) {
                return Err(agent_failure("agents.foreign", AgentId::Cursor));
            }
            if command == self.selected.command
                && args == self.selected.args
                && cursor_env(current) == self.mcp_env()
            {
                return Ok(());
            }
        }
        let mut servers = servers;
        let mut entry = servers
            .get("sidevoice")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        entry.insert("command".into(), json!(self.selected.command));
        entry.insert("args".into(), json!(self.selected.args));
        entry.insert("env".into(), json!(self.mcp_env()));
        servers.insert("sidevoice".into(), Value::Object(entry));
        config["mcpServers"] = Value::Object(servers);
        write_cursor_config(&self.profile, &file, &config)
            .map_err(|_| agent_failure("agents.invalid", AgentId::Cursor))
    }

    pub(super) fn cursor_disconnect(&self) -> std::result::Result<(), Failure> {
        self.profile
            .validate_for_agent(&self.profile.cursor)
            .map_err(|_| agent_failure("agents.invalid", AgentId::Cursor))?;
        let file = self.profile.cursor.join("mcp.json");
        let root = self
            .profile
            .agent_root(&self.profile.cursor)
            .map_err(|_| agent_failure("agents.invalid", AgentId::Cursor))?;
        let Some((mut config, _, _)) = read_cursor_config(&file, &root)
            .map_err(|_| agent_failure("agents.invalid", AgentId::Cursor))?
        else {
            return Ok(());
        };
        let Some(servers) = config.get_mut("mcpServers").and_then(Value::as_object_mut) else {
            return Ok(());
        };
        let Some(current) = servers.get("sidevoice") else {
            return Ok(());
        };
        let command = current.get("command").and_then(Value::as_str).unwrap_or("");
        let args = current
            .get("args")
            .and_then(Value::as_array)
            .and_then(|values| {
                values
                    .iter()
                    .map(|value| value.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
            })
            .unwrap_or_default();
        if !self.selected.owns(command, &args) {
            return Err(agent_failure("agents.foreign", AgentId::Cursor));
        }
        servers.remove("sidevoice");
        write_cursor_config(&self.profile, &file, &config)
            .map_err(|_| agent_failure("agents.invalid", AgentId::Cursor))
    }
}

/// The environment a Cursor entry gives its server.
fn cursor_env(entry: &Value) -> BTreeMap<String, String> {
    entry
        .get("env")
        .and_then(Value::as_object)
        .map(|env| {
            env.iter()
                .map(|(key, value)| (key.clone(), value.as_str().unwrap_or("").to_owned()))
                .collect()
        })
        .unwrap_or_default()
}
