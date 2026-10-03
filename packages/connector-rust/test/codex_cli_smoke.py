"""Actual Codex CLI registration smoke, confined to a disposable profile."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import tomllib


def run(command, env, expect=0):
    result = subprocess.run(command, env=env, capture_output=True, text=True, timeout=20)
    if result.returncode != expect:
        raise AssertionError(
            f"command exited {result.returncode}, expected {expect}: {command!r}\n"
            f"stdout: {result.stdout[-1000:]}\nstderr: {result.stderr[-1000:]}"
        )
    return result


def rust_action(binary, action, root, env, expect=0):
    result = run([binary, "codex", action, "--profile-root", str(root)], env, expect)
    if not result.stdout.strip():
        raise AssertionError(f"Rust {action} returned no JSON: {result.stderr[-1000:]}")
    try:
        return json.loads(result.stdout.splitlines()[-1])
    except json.JSONDecodeError as error:
        raise AssertionError(f"Rust {action} returned invalid JSON: {result.stdout[-1000:]}") from error


def main():
    binary = str(Path(os.environ["SIDEVOICE_RUST_PROOF_BIN"]).resolve())
    codex = str(Path(os.environ["SIDEVOICE_CODEX_BIN"]).resolve())
    safe_path = os.environ.get("PATH", "/usr/local/bin:/usr/bin:/bin")
    default_config = Path.home() / ".codex" / "config.toml"
    default_before = default_config.read_bytes() if default_config.exists() else None

    with tempfile.TemporaryDirectory(prefix="sidevoice-codex-smoke-") as temporary:
        root = Path(temporary).resolve()
        root.chmod(0o700)
        paths = {name: root / name for name in ("home", "claude", "codex", "cursor", "xdg", "sidevoice")}
        for path in paths.values():
            path.mkdir(mode=0o700)
        for name in ("cursor/config", "cursor/data", "xdg/config", "xdg/data"):
            (root / name).mkdir(mode=0o700)
        (paths["sidevoice"] / "core").mkdir(mode=0o700)
        env = {
            "PATH": safe_path,
            "HOME": str(paths["home"]),
            "CLAUDE_CONFIG_DIR": str(paths["claude"]),
            "CODEX_HOME": str(paths["codex"]),
            "CURSOR_CONFIG_DIR": str(root / "cursor/config"),
            "CURSOR_DATA_DIR": str(root / "cursor/data"),
            "XDG_CONFIG_HOME": str(root / "xdg/config"),
            "XDG_DATA_HOME": str(root / "xdg/data"),
            "SIDEVOICE_DATA_DIR": str(paths["sidevoice"]),
            "SIDEVOICE_CODEX_BIN": codex,
            "SIDEVOICE_SERVICE_MANAGER": "none",
        }

        absent = rust_action(binary, "inspect", root, env)
        codex_row = next(agent for agent in absent["agents"] if agent["id"] == "codex")
        assert codex_row["registration"] == "not-connected"

        rust_action(binary, "connect", root, env)
        owned = json.loads(run([codex, "mcp", "get", "sidevoice", "--json"], env).stdout)
        transport = owned["transport"]
        assert transport["type"] == "stdio"
        assert transport["command"] == binary
        assert transport["args"] == ["mcp", "--profile-root", str(root)]
        connected = rust_action(binary, "inspect", root, env)
        assert next(agent for agent in connected["agents"] if agent["id"] == "codex")["registration"] == "connected"
        run([codex, "mcp", "remove", "sidevoice"], env)
        codex_config = paths["codex"] / "config.toml"
        if codex_config.exists():
            assert "sidevoice" not in tomllib.loads(codex_config.read_text()).get("mcp_servers", {})

        run([codex, "mcp", "add", "sidevoice", "--", "/bin/echo", "keep-existing"], env)
        foreign_path = paths["codex"] / "config.toml"
        foreign_before = foreign_path.read_bytes()
        refused = rust_action(binary, "connect", root, env, expect=1)
        assert refused["error"]["key"] == "agents.foreign"
        assert foreign_path.read_bytes() == foreign_before
        foreign = json.loads(run([codex, "mcp", "get", "sidevoice", "--json"], env).stdout)
        assert foreign["transport"]["command"] == "/bin/echo"
        run([codex, "mcp", "remove", "sidevoice"], env)

        invalid_before = b"[mcp_servers.sidevoice\ncommand = [broken\n"
        foreign_path.write_bytes(invalid_before)
        foreign_path.chmod(0o600)
        refused = rust_action(binary, "connect", root, env, expect=1)
        assert refused["error"]["key"] == "agents.invalid"
        assert foreign_path.read_bytes() == invalid_before

        assert not (paths["sidevoice"] / "install.json").exists()
        assert default_config.read_bytes() == default_before if default_before is not None else not default_config.exists()
        print(json.dumps({
            "actual_codex_cli": "0.160.0",
            "proof_binary": binary,
            "isolated_home": True,
            "rust_add_get_inspect": True,
            "foreign_preserved": True,
            "invalid_preserved": True,
            "default_codex_home_unchanged": True,
        }))


if __name__ == "__main__":
    main()
