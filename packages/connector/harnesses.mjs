/** Registry and selection for the harness modules. The façade and connector ask this registry;
 *  neither contains harness-name branches. */
import { claudeHarness } from './harness-claude.mjs';
import { codexHarness } from './harness-codex.mjs';
import { httpHarness } from './harness-http.mjs';

export const harnesses = Object.freeze({ claude: claudeHarness, codex: codexHarness, http: httpHarness });

export function harnessFor(name) {
  return harnesses[name] || httpHarness;
}

export function identifyHarness(meta, env = process.env) {
  for (const harness of [claudeHarness, codexHarness, httpHarness]) {
    const identity = harness.sessionIdentity({ meta, env });
    if (identity?.delivery) return { ...identity, module: harness };
  }
  throw new Error('Cannot tell which conversation this is: not launched by Claude Code or Codex, and no SIDEVOICE_THREAD/SIDEVOICE_DELIVERY_URL set');
}

/** Which harness a hook invocation belongs to.
 *
 *  The installed hook command names it (`sidevoice hook --harness <name>`, or SIDEVOICE_HOOK_HARNESS):
 *  a harness started from another harness's terminal inherits that one's environment variables, and both
 *  hook payloads carry a `session_id`, so neither the environment nor the payload can tell them apart on
 *  its own. Naming it at the point of install is mechanical; guessing is not. Without a declaration the
 *  modules are asked in order, which only holds when a single harness runs on the machine. */
export function identifyHookHarness(payload, env = process.env) {
  const declared = env.SIDEVOICE_HOOK_HARNESS;
  const candidates = declared ? [harnesses[declared]].filter(Boolean) : [claudeHarness, codexHarness, httpHarness];
  for (const harness of candidates) {
    const identity = harness.sessionIdentity({ payload, env });
    if (identity) return { ...identity, module: harness };
  }
  return null;
}
