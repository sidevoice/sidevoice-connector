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

export function identifyHookHarness(payload, env = process.env) {
  for (const harness of [claudeHarness, codexHarness, httpHarness]) {
    const identity = harness.sessionIdentity({ payload, env });
    if (identity) return { ...identity, module: harness };
  }
  return null;
}
