import test from 'node:test';
import assert from 'node:assert/strict';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { connectorMcpCommand } from '../agent-support.mjs';
import { agentAction, handleAgentRequest, listAgents } from '../agents.mjs';
import { nodeFiles, readJson, writePrivate } from '../node-files.mjs';

function executable(file, source) {
  writeFileSync(file, source, { mode: 0o700 });
  chmodSync(file, 0o700);
}

function fixture() {
  const home = mkdtempSync(path.join(os.tmpdir(), 'sv-agents-'));
  const binDir = path.join(home, 'login-bin'); mkdirSync(binDir);
  const claudeDir = path.join(home, '.claude'); mkdirSync(claudeDir);
  const codexDir = path.join(home, '.codex'); mkdirSync(codexDir);
  const cursorDir = path.join(home, '.cursor'); mkdirSync(cursorDir);
  const dataDir = path.join(home, '.sidevoice'); mkdirSync(dataDir, { mode: 0o700 });
  const releases = path.join(home, 'share', 'sidevoice');
  const cli = path.join(releases, 'current', 'dist', 'cli.mjs'); mkdirSync(path.dirname(cli), { recursive: true }); writeFileSync(cli, '');
  writePrivate(nodeFiles(dataDir).install, { command: [process.execPath, cli], releases, definitions: [] });
  const shell = path.join(home, 'login-shell');
  executable(shell, `#!/bin/sh\nprintf '%s' "$AGENT_TEST_LOGIN_PATH"\n`);
  const claudeFile = path.join(home, 'claude-registration.txt');
  const claude = path.join(binDir, 'claude');
  executable(claude, `#!/bin/sh
if [ "$1" = "--version" ]; then echo 'Claude Code 1.4.0'; exit 0; fi
if [ "$1" = "mcp" ] && [ "$2" = "get" ]; then [ -s ${JSON.stringify(claudeFile)} ] && cat ${JSON.stringify(claudeFile)} || exit 1; exit 0; fi
if [ "$1" = "mcp" ] && [ "$2" = "remove" ]; then rm -f ${JSON.stringify(claudeFile)}; exit 0; fi
if [ "$1" = "mcp" ] && [ "$2" = "add" ]; then
  if [ "$FAIL_CLAUDE_ADD" = "1" ]; then echo 'private sentinel from claude' >&2; exit 7; fi
  shift 6
  command="$1"; shift
  printf 'sidevoice:\\n  Scope: User config\\n  Type: stdio\\n  Command: %s\\n  Args: %s\\n' "$command" "$*" > ${JSON.stringify(claudeFile)}
  exit 0
fi
exit 2
`);
  const codexConfig = path.join(codexDir, 'config.toml');
  const codexInvocations = path.join(home, 'codex-invocations.txt');
  const codex = path.join(binDir, 'codex');
  executable(codex, `#!/bin/sh
if [ -n "$CODEX_INVOCATIONS_FILE" ]; then printf '%s\\n' "$*" >> "$CODEX_INVOCATIONS_FILE"; fi
if [ "$SLOW_CODEX" = "1" ]; then sleep 2; fi
if [ "$1" = "--version" ]; then echo 'codex-cli 0.157.0'; exit 0; fi
if [ "$1" = "mcp" ] && [ "$2" = "get" ] && [ "$3" = "sidevoice" ] && [ "$4" = "--json" ]; then
  if [ -n "$CODEX_GET_MARKER" ]; then : > "$CODEX_GET_MARKER"; fi
  if [ -n "$CODEX_GET_DELAY" ]; then sleep "$CODEX_GET_DELAY"; fi
  if [ ! -s ${JSON.stringify(codexConfig)} ]; then echo 'No MCP server named sidevoice' >&2; exit 1; fi
  command=$(sed -n 's/^command = "\\(.*\\)"/\\1/p' ${JSON.stringify(codexConfig)})
  args=$(sed -n 's/^args = \\["\\(.*\\)", "\\(.*\\)"\\]/\\1 \\2/p' ${JSON.stringify(codexConfig)})
  set -- $args
  printf '{"name":"sidevoice","enabled":true,"transport":{"type":"stdio","command":"%s","args":["%s","%s"]}}\\n' "$command" "$1" "$2"
  exit 0
fi
if [ "$1" = "mcp" ] && [ "$2" = "add" ]; then
  printf '[mcp_servers.sidevoice]\\ncommand = "%s"\\nargs = ["%s", "%s"]\\n' "$5" "$6" "$7" > ${JSON.stringify(codexConfig)}
  exit 0
fi
if [ "$1" = "mcp" ] && [ "$2" = "remove" ]; then rm -f ${JSON.stringify(codexConfig)}; exit 0; fi
exit 2
`);
  const cursorVersion = path.join(home, 'cursor-version'); writeFileSync(cursorVersion, 'Cursor 2.0.0\n');
  const cursor = path.join(binDir, 'cursor-agent');
  executable(cursor, `#!/bin/sh
if [ "$1" = "--version" ]; then cat ${JSON.stringify(cursorVersion)}; exit 0; fi
exit 2
`);
  const env = { ...process.env, HOME: home, SHELL: shell, AGENT_TEST_LOGIN_PATH: binDir, SIDEVOICE_DATA_DIR: dataDir,
    XDG_DATA_HOME: path.join(home, 'share'), CLAUDE_CONFIG_DIR: claudeDir, CODEX_HOME: codexDir, CURSOR_CONFIG_DIR: cursorDir,
    SIDEVOICE_AGENT_TIMEOUT_MS: '1000' };
  for (const key of ['SIDEVOICE_URL', 'SIDEVOICE_CONNECTOR_ID', 'SIDEVOICE_CONNECTOR_TOKEN']) delete env[key];
  return { home, binDir, claudeDir, codexDir, cursorDir, dataDir, claude, codex, cursor, cursorVersion, codexConfig, codexInvocations, env };
}

test('host scan captures login-shell PATH and returns agent-specific manual configuration from the installed command', () => {
  const f = fixture();
  const result = listAgents(f.env, { rescan: true });
  assert.equal(result.agents.length, 3);
  assert.deepEqual(result.agents.map(agent => agent.id), ['claude', 'codex', 'cursor']);
  assert.deepEqual(result.agents.map(agent => agent.registration), ['not-connected', 'not-connected', 'not-connected']);
  assert.deepEqual(result.agents.map(agent => agent.version), ['Claude Code 1.4.0', 'codex-cli 0.157.0', 'Cursor 2.0.0']);
  assert.ok(result.custom.command.includes(path.join(f.home, 'share', 'sidevoice', 'current', 'dist', 'cli.mjs')));
  assert.deepEqual(JSON.parse(result.custom.snippet).mcpServers.sidevoice.args,
    [path.join(f.home, 'share', 'sidevoice', 'current', 'dist', 'cli.mjs'), 'mcp']);
  assert.match(result.agents.find(agent => agent.id === 'claude').instructions.command, /mcp' 'add/);
  assert.match(result.agents.find(agent => agent.id === 'codex').instructions.snippet, /\[mcp_servers\.sidevoice\]/);
  assert.deepEqual(JSON.parse(result.agents.find(agent => agent.id === 'cursor').instructions.snippet).mcpServers.sidevoice.args,
    [path.join(f.home, 'share', 'sidevoice', 'current', 'dist', 'cli.mjs'), 'mcp']);
  const saved = readJson(nodeFiles(f.dataDir).agents);
  assert.equal(saved.login_path, f.binDir);
  assert.equal(saved.binaries.claude, f.claude);
  assert.equal(saved.version, 1);
});

test('explicit connect and disconnect use each harness, and foreign entries stay untouched', () => {
  const f = fixture();
  listAgents(f.env, { rescan: true });
  for (const id of ['claude', 'codex', 'cursor']) {
    const connected = agentAction('connect', id, f.env);
    assert.equal(connected.agents.find(agent => agent.id === id).registration, 'connected', `${id} connects`);
    const disconnected = agentAction('disconnect', id, f.env);
    assert.equal(disconnected.agents.find(agent => agent.id === id).registration, 'not-connected', `${id} disconnects`);
  }

  const foreign = { mcpServers: { sidevoice: { command: '/usr/local/bin/other-mcp', args: ['--serve'] } } };
  writeFileSync(path.join(f.cursorDir, 'mcp.json'), JSON.stringify(foreign));
  assert.throws(() => agentAction('connect', 'cursor', f.env), error => error.key === 'agents.foreign');
  assert.deepEqual(JSON.parse(readFileSync(path.join(f.cursorDir, 'mcp.json'), 'utf8')), foreign);
});

test('dismissal survives rescans and restarts, then a missing agent reappearing gets a new notice generation', () => {
  const f = fixture();
  const first = listAgents(f.env, { rescan: true }).agents.find(agent => agent.id === 'cursor');
  assert.equal(first.actionable, true);
  const dismissed = agentAction('dismiss', 'cursor', f.env).agents.find(agent => agent.id === 'cursor');
  assert.equal(dismissed.dismissed, true);
  assert.equal(dismissed.actionable, false);
  assert.equal(listAgents(f.env, { rescan: true }).agents.find(agent => agent.id === 'cursor').actionable, false);
  const saved = readJson(nodeFiles(f.dataDir).agents);
  assert.equal(saved.dismissed.cursor, saved.seen.cursor.generation);

  rmSync(f.cursorDir, { recursive: true, force: true });
  rmSync(f.cursor, { force: true });
  assert.equal(listAgents(f.env, { rescan: true }).agents.some(agent => agent.id === 'cursor'), false);
  mkdirSync(f.cursorDir);
  executable(f.cursor, `#!/bin/sh\nif [ "$1" = "--version" ]; then cat ${JSON.stringify(f.cursorVersion)}; exit 0; fi\nexit 2\n`);
  const returned = listAgents(f.env, { rescan: true }).agents.find(agent => agent.id === 'cursor');
  assert.notEqual(readJson(nodeFiles(f.dataDir).agents).seen.cursor.generation, saved.seen.cursor.generation);
  assert.equal(returned.actionable, true);
  assert.equal(returned.dismissed, false);
});

test('connector link agent requests accept only registered ids and return keyed errors', () => {
  const f = fixture();
  const result = handleAgentRequest('agents.list', { rescan: true }, f.env);
  assert.deepEqual(Object.keys(result).sort(), ['agents', 'custom', 'scanned_at']);
  assert.equal(handleAgentRequest('agents.connect', { id: '/tmp/command' }, f.env).error.key, 'agents.unknown');
});

test('agent action errors keep subprocess output in the private error callback, not in the link reply', () => {
  const f = fixture();
  f.env.FAIL_CLAUDE_ADD = '1';
  const privateErrors = [];
  const result = handleAgentRequest('agents.connect', { id: 'claude' }, f.env, error => privateErrors.push(error));
  assert.equal(result.error.key, 'agents.action-failed');
  assert.equal(result.error.message, 'Could not update Claude Code agent configuration.');
  assert.doesNotMatch(JSON.stringify(result.error), /private sentinel|Command failed/);
  assert.match(String(privateErrors[0]?.stderr), /private sentinel from claude/);
});

test('manual MCP command uses the current CLI before install.json exists', () => {
  const home = mkdtempSync(path.join(os.tmpdir(), 'sv-agents-uninstalled-'));
  const dataDir = path.join(home, '.sidevoice'); mkdirSync(dataDir, { mode: 0o700 });
  const result = connectorMcpCommand({ ...process.env, HOME: home, SIDEVOICE_DATA_DIR: dataDir });
  assert.equal(result.command, process.execPath);
  assert.equal(result.args.at(-1), 'mcp');
  assert.match(result.args[0], /(?:^|[\\/])cli\.mjs$/);
  assert.equal(existsSync(result.args[0]), true);
  assert.ok(result.version);
});

test('a slow Codex CLI stays within the core request bound for explicit connect', () => {
  const f = fixture();
  f.env.SLOW_CODEX = '1';
  f.env.SIDEVOICE_AGENT_TIMEOUT_MS = '2500';
  f.env.CODEX_INVOCATIONS_FILE = f.codexInvocations;
  const started = Date.now();
  const connected = agentAction('connect', 'codex', f.env);
  const elapsed = Date.now() - started;
  assert.equal(connected.agents.find(agent => agent.id === 'codex').registration, 'connected');
  assert.deepEqual(readFileSync(f.codexInvocations, 'utf8').trim().split('\n').map(line => line.split(' ').slice(0, 3).join(' ')), [
    'mcp get sidevoice', '--version', 'mcp get sidevoice', 'mcp add sidevoice', 'mcp get sidevoice',
  ]);
  assert.ok(elapsed < 20_000, `explicit connect took ${elapsed} ms, over the core's 20 s request bound`);
});

test('a detached scan cannot overwrite a dismissal committed while Codex detection is running', async () => {
  const f = fixture();
  listAgents(f.env, { rescan: true });
  const marker = path.join(f.home, 'codex-get-started');
  f.env.CODEX_GET_MARKER = marker;
  f.env.CODEX_GET_DELAY = '1';
  const cli = new URL('../cli.mjs', import.meta.url).pathname;
  const child = spawn(process.execPath, [cli, '--sidevoice-agent-scan'], { env: f.env, stdio: 'ignore' });
  const waitForChild = new Promise((resolve, reject) => {
    child.once('error', reject);
    child.once('exit', (code, signal) => code === 0 ? resolve() : reject(new Error(`agent scan exited ${code ?? signal}`)));
  });
  try {
    const deadline = Date.now() + 5_000;
    while (!existsSync(marker) && Date.now() < deadline) await new Promise(resolve => setTimeout(resolve, 10));
    assert.equal(existsSync(marker), true, 'background scan reached the slow Codex detection');
    const dismissed = agentAction('dismiss', 'cursor', f.env).agents.find(agent => agent.id === 'cursor');
    assert.equal(dismissed.dismissed, true);
    await waitForChild;
    const afterScan = listAgents(f.env, { rescan: false }).agents.find(agent => agent.id === 'cursor');
    assert.equal(afterScan.dismissed, true);
    assert.equal(afterScan.actionable, false);
  } finally {
    if (child.exitCode === null && child.signalCode === null) child.kill('SIGTERM');
  }
});

test('a detached scan finishing after uninstall does not recreate agents.json', async () => {
  const f = fixture();
  listAgents(f.env, { rescan: true });
  const marker = path.join(f.home, 'codex-get-started');
  f.env.CODEX_GET_MARKER = marker;
  f.env.CODEX_GET_DELAY = '2';
  const cli = new URL('../cli.mjs', import.meta.url).pathname;
  const child = spawn(process.execPath, [cli, '--sidevoice-agent-scan'], { env: f.env, stdio: 'ignore' });
  const waitForChild = new Promise((resolve, reject) => {
    child.once('error', reject);
    child.once('exit', (code, signal) => code === 0 ? resolve() : reject(new Error(`agent scan exited ${code ?? signal}`)));
  });
  try {
    const deadline = Date.now() + 5_000;
    while (!existsSync(marker) && Date.now() < deadline) await new Promise(resolve => setTimeout(resolve, 10));
    assert.equal(existsSync(marker), true, 'background scan reached the slow Codex detection');
    const { uninstall } = await import('../install.mjs');
    await uninstall(['--harness', 'cursor'], f.env);
    assert.equal(existsSync(nodeFiles(f.dataDir).agents), false, 'uninstall removed the persisted agent scan');
    assert.equal(child.exitCode, null, 'scanner is still blocked when uninstall finishes');
    await waitForChild;
    assert.equal(existsSync(nodeFiles(f.dataDir).agents), false, 'late scanner commit did not recreate agents.json');
    assert.equal(existsSync(nodeFiles(f.dataDir).agentsLock), true, 'the coordination lock inode remains permanent');
  } finally {
    if (child.exitCode === null && child.signalCode === null) child.kill('SIGTERM');
  }
});
