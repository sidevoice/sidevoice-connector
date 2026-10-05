// How to point each harness at the connector under test without touching its real configuration. Everything the
// bench writes lives under the profile root, so deleting that directory undoes it.
//
// - Claude Code: a session-scoped `--mcp-config` file with `--strict-mcp-config`; the user's own config is read
//   for login but never written, and no other MCP server (a real Sidevoice included) is loaded.
// - Codex: a private CODEX_HOME inside the profile, the one the connector also hands to `codex queue`.
// - Cursor CLI: a scratch project whose `.cursor/mcp.json` names the server; nothing in ~/.cursor changes.

import { mkdirSync, writeFileSync } from 'node:fs';
import path from 'node:path';

const quote = (value) => (/^[A-Za-z0-9_./:=@-]+$/.test(value) ? value : `'${value.replaceAll("'", "'\\''")}'`);

export function harnessSetup({ binary, paths }) {
  const server = { command: binary, args: ['--profile-root', paths.root, 'mcp'] };
  const claudeConfig = path.join(paths.bench, 'claude-mcp.json');
  const cursorProject = path.join(paths.bench, 'cursor-project');
  const commandLine = [binary, ...server.args].map(quote).join(' ');
  return {
    server,
    claude: {
      files: { [claudeConfig]: { mcpServers: { sidevoice: server } } },
      run: `claude --mcp-config ${quote(claudeConfig)} --strict-mcp-config`,
      cleanup: null,
    },
    codex: {
      files: {},
      login: `CODEX_HOME=${quote(paths.codex)} codex login`,
      register: `CODEX_HOME=${quote(paths.codex)} codex mcp add sidevoice -- ${commandLine}`,
      run: `CODEX_HOME=${quote(paths.codex)} codex`,
      cleanup: `CODEX_HOME=${quote(paths.codex)} codex mcp remove sidevoice`,
    },
    cursor: {
      files: { [path.join(cursorProject, '.cursor', 'mcp.json')]: { mcpServers: { sidevoice: server } } },
      run: `cd ${quote(cursorProject)} && cursor-agent persist`,
      cleanup: null,
    },
    http: {
      files: {},
      run: `SIDEVOICE_THREAD=my-thread SIDEVOICE_DELIVERY_URL=http://127.0.0.1:<port>/ ${commandLine}`,
      cleanup: null,
    },
    reset: `rm -rf ${quote(paths.root)}`,
  };
}

/** Writes the per-harness files (only under the profile root). */
export function writeHarnessFiles(setup) {
  for (const harness of ['claude', 'codex', 'cursor', 'http']) {
    for (const [file, content] of Object.entries(setup[harness].files)) {
      mkdirSync(path.dirname(file), { recursive: true, mode: 0o700 });
      writeFileSync(file, `${JSON.stringify(content, null, 2)}\n`, { mode: 0o600 });
    }
  }
}
