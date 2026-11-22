#!/usr/bin/env node
// Plays the scripted conversations against the real MCP instructions and tool schemas (stub-mcp.mjs) through the
// `claude` CLI in headless mode, one process per conversation, and keeps each run as JSON in <out>/raw.
//   node run.mjs --out DIR [--models a,b] [--styles brief,normal,detailed] [--variants current,mandatory,tags,afterheader,atend]
//                [--scripts b,c,e,f,g] [--reps 3] [--parallel 3] [--budget USD: stop starting runs past it]
// Runs that already have a raw file are skipped, so an interrupted matrix resumes.
import { spawn } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { notes, scripts, variants } from "./scripts.mjs";

const here = dirname(fileURLToPath(import.meta.url));
export const MODELS = { haiku: "claude-haiku-5-5", sonnet: "claude-sonnet-5-5", opus: "claude-opus-5-5" };
const SYSTEM = `You are a coding assistant working in the person's repository. Your final text is the written reply, shown on screen.\n\n${notes}`;

export function envelope(turn, n, style, variant, lastSay) {
  const t = typeof turn === "string" ? { text: turn } : turn;
  const channel = t.note ? "note" : "voice";
  let tag = `[sidevoice channel=${channel} session_id=s1 revision=${n} epoch=${n} message_id=${channel === "note" ? "note:" : "m-"}${n} style=${style}`;
  if (t.unheard) {
    const from = (lastSay ?? "").split(/\s+/).slice(0, 6).join(" ");
    tag += ` unheard=${t.unheard} unheard_from=${JSON.stringify(from + "…")}`;
  }
  const v = variants[variant];
  if (v.headerTags) tag += ` ${v.headerTags}`;
  tag += "]";
  return [tag, ...(v.afterHeader ? [v.afterHeader] : []), ...(t.note ? [] : [t.text]), ...(v.tail ? [v.tail] : [])].join("\n\n");
}

function conversation({ model, script, style, variant }) {
  const cwd = mkdtempSync(join(tmpdir(), "adherence-"));
  const mcp = join(cwd, "mcp.json");
  writeFileSync(mcp, JSON.stringify({ mcpServers: { sidevoice: { command: "node", args: [join(here, "stub-mcp.mjs")] } } }));
  const child = spawn("claude", ["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose",
    "--model", MODELS[model], "--system-prompt", SYSTEM, "--tools", "", "--mcp-config", mcp, "--strict-mcp-config",
    "--permission-mode", "bypassPermissions", "--no-session-persistence", "--setting-sources", ""],
    { cwd, env: { ...process.env, CLAUDE_CODE_DISABLE_CLAUDE_MDS: "1" }, stdio: ["pipe", "pipe", "pipe"] });
  const waiting = [];
  let buf = "", stderr = "", events = [];
  child.stderr.on("data", (d) => (stderr += d));
  child.stdout.on("data", (d) => {
    buf += d;
    for (let i; (i = buf.indexOf("\n")) >= 0; ) {
      const line = buf.slice(0, i); buf = buf.slice(i + 1);
      let ev; try { ev = JSON.parse(line); } catch { continue; }
      events.push(ev);
      if (ev.type === "result") waiting.shift()?.(ev);
    }
  });
  child.on("exit", () => { while (waiting.length) waiting.shift()(null); });
  const turn = (content) => new Promise((resolve) => {
    const timer = setTimeout(() => resolve(null), 300000);
    waiting.push((ev) => { clearTimeout(timer); resolve(ev); });
    child.stdin.write(JSON.stringify({ type: "user", message: { role: "user", content } }) + "\n");
  });
  return { turn, events: () => events, stderr: () => stderr, close: () => { child.stdin.end(); setTimeout(() => child.kill(), 3000); } };
}

/** The ordered things the agent did since `from` in the stream: voice_say texts, other tool calls, the written text. */
function actions(events, from) {
  const out = [];
  for (const ev of events.slice(from)) {
    if (ev.type !== "assistant") continue;
    for (const b of ev.message.content) {
      if (b.type === "tool_use" && b.name.endsWith("voice_say")) out.push({ kind: "say", text: b.input?.text ?? "" });
      else if (b.type === "tool_use") out.push({ kind: "tool", name: b.name.replace(/^mcp__sidevoice__/, ""), input: b.input });
      else if (b.type === "text" && b.text.trim()) out.push({ kind: "written", text: b.text.trim() });
    }
  }
  return out;
}

export async function play(run) {
  const script = scripts[run.script];
  const c = conversation(run);
  const turns = [];
  let lastSay = "", cost = 0;
  const usage = { input: 0, output: 0, cache_read: 0, cache_write: 0 };
  let ms = 0, apiTurns = 0;
  for (const [i, spec] of script.turns.entries()) {
    const user = spec.compact ? "/compact" : envelope(spec, i + 1, run.style, run.variant, lastSay);
    const from = c.events().length;
    const res = await c.turn(user);
    const acts = actions(c.events(), from);
    const say = acts.filter((a) => a.kind === "say").at(-1);
    if (say) lastSay = say.text;
    if (res) {
      cost = res.total_cost_usd ?? cost;
      ms += res.duration_ms ?? 0; apiTurns += res.num_turns ?? 0;
      const u = res.usage ?? {};
      usage.input += u.input_tokens ?? 0; usage.output += u.output_tokens ?? 0;
      usage.cache_read += u.cache_read_input_tokens ?? 0; usage.cache_write += u.cache_creation_input_tokens ?? 0;
    }
    const t = typeof spec === "string" ? { text: spec } : spec;
    turns.push({ n: i + 1, user, person: spec.compact ? "/compact" : t.text ?? "", compact: !!spec.compact, ms: res?.duration_ms, api_turns: res?.num_turns, usage: res?.usage, result: spec.compact ? (res?.result ?? "").slice(0, 300) : undefined, note: !!t.note, speak: t.speak ?? true, ask: t.ask, unheard: t.unheard, actions: acts, failed: !res || res.is_error });
    if (!res) break;
  }
  c.close();
  return { ...run, turns, cost_usd: cost, usage, ms, api_turns: apiTurns, stderr: c.stderr().slice(0, 500) };
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const arg = (k, d) => { const i = process.argv.indexOf(`--${k}`); return i < 0 ? d : process.argv[i + 1]; };
  const out = arg("out"); if (!out) throw new Error("--out DIR");
  const list = (k, d) => arg(k, d).split(",");
  mkdirSync(join(out, "raw"), { recursive: true });
  const jobs = [];
  for (const model of list("models", Object.keys(MODELS).join(",")))
    for (const style of list("styles", "normal"))
      for (const variant of list("variants", Object.keys(variants).join(",")))
        for (const script of list("scripts", "b,c,e,f,g"))
          for (let rep = 1; rep <= Number(arg("reps", 3)); rep++) {
            const file = join(out, "raw", `${model}__${style}__${variant}__${script}__r${rep}.json`);
            if (!existsSync(file)) jobs.push({ file, run: { model, style, variant, script, rep } });
          }
  jobs.sort((a, b) => a.run.rep - b.run.rep); // balanced: every cell gets its first repetition before any gets its second
  const budget = Number(arg("budget", Infinity));
  let next = 0, done = 0, spent = 0;
  await Promise.all(Array.from({ length: Number(arg("parallel", 3)) }, async () => {
    while (next < jobs.length && spent < budget) {
      const { file, run } = jobs[next++];
      const result = await play(run);
      writeFileSync(file, JSON.stringify(result, null, 1));
      spent += result.cost_usd;
      console.log(`[${++done}/${jobs.length}] ${file.split("/").pop()} \$${result.cost_usd.toFixed(3)} (total \$${spent.toFixed(2)})${result.turns.some((t) => t.failed) ? " FAILED" : ""}`);
    }
  }));
}
