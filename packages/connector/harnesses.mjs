/** Registry and selection for the harness modules. The façade and connector ask this registry;
 *  neither contains harness-name branches. */
import { claudeHarness } from './harness-claude.mjs';
import { codexHarness } from './harness-codex.mjs';
import { cursorHarness, isCursorClient } from './harness-cursor.mjs';
import { httpHarness } from './harness-http.mjs';

export const harnesses = Object.freeze({ claude: claudeHarness, codex: codexHarness, cursor: cursorHarness, http: httpHarness });

export function harnessFor(name) {
  return harnesses[name] || httpHarness;
}

/** `client` is what the harness said about itself in `initialize` (`clientInfo`): the only thing Cursor
 *  says, since it passes its MCP servers neither environment nor metadata about the conversation. */
export function identifyHarness(meta, env = process.env, client = null) {
  // A client that says it is Cursor is Cursor, whatever it inherited: the editor passes its whole environment
  // on, so a Claude Code or Codex session that opened it leaves its ids there. An explicitly configured
  // receiver still comes first; Cursor refuses a conversation it cannot place.
  const order = isCursorClient(client) ? [httpHarness, cursorHarness] : [claudeHarness, codexHarness, httpHarness, cursorHarness];
  for (const harness of order) {
    const identity = harness.sessionIdentity({ meta, env, client });
    if (identity?.delivery) return { ...identity, module: harness };
  }
  throw new Error('Cannot tell which conversation this is: not launched by Claude Code, Codex or the Cursor CLI, and no SIDEVOICE_THREAD/SIDEVOICE_DELIVERY_URL set');
}
