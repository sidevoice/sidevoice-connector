#!/usr/bin/env node
// A stand-in for the Sidevoice MCP server: the exact server instructions and tool schemas, read from
// connector/src/mcp.rs, and no stack behind them. voice_say appends its call to $ADHERENCE_LOG (JSONL) and says it was
// published; the other tools answer with a canned "nothing" so the model can carry on.
import { appendFileSync, readFileSync } from "node:fs";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

export function loadSpec(path = join(dirname(fileURLToPath(import.meta.url)), "../../connector/src/mcp.rs")) {
  const src = readFileSync(path, "utf8");
  const instructions = /const INSTRUCTIONS: &str = r#"([\s\S]*?)"#;/.exec(src)?.[1];
  if (!instructions) throw new Error("INSTRUCTIONS not found in mcp.rs");
  const tools = [];
  const re = /^\s*\("(\w+)", "((?:[^"\\]|\\.)*)", json!\((\{.*\})\), vec!\[(.*?)\]\),$/gm;
  for (const m of src.matchAll(re)) {
    tools.push({
      name: m[1],
      description: JSON.parse(`"${m[2]}"`),
      inputSchema: {
        type: "object",
        properties: JSON.parse(m[3]),
        required: [...m[4].matchAll(/"(\w+)"/g)].map((r) => r[1]),
      },
    });
  }
  if (tools.length < 7 || !tools.some((t) => t.name === "voice_say")) throw new Error("tool specs in mcp.rs changed shape");
  return { instructions, tools };
}

const canned = {
  get_messages: { messages: [], note: "nothing new" },
  voice_status: { joined: true, reachable: true },
  voice_connect: { joined: true },
  voice_disconnect: { ok: true },
  voice_pair_device: { ok: false, reason: "not in the test" },
  voice_diagnose: { ok: true, note: "nothing wrong" },
};
// A fake repository, enough for the model to carry on: it answers by what the command looks like.
function shellOutput(command = "") {
  if (/release/.test(command)) return "test result: FAILED. 211 passed; 2 failed; finished in 58.9s\nfailures: auth::token_refresh, config::reload";
  if (/test|cargo/.test(command)) return "test result: ok. 213 passed; 0 failed; finished in 41.2s";
  if (/ls|find|tree/.test(command)) return "Cargo.toml\nconnector/src/auth.rs\nconnector/src/config.rs\nconnector/tests/auth.rs";
  if (/cat|sed|grep|rg/.test(command)) return "// (file contents elided in the test)";
  return "ok";
}
const shell = {
  name: "run_shell",
  description: "Run a shell command in the repository and return its output.",
  inputSchema: { type: "object", properties: { command: { type: "string" } }, required: ["command"] },
};

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const { instructions, tools: sidevoiceTools } = loadSpec();
  const tools = [...sidevoiceTools, shell];
  const send = (id, result) => process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id, result }) + "\n");
  createInterface({ input: process.stdin }).on("line", (line) => {
    const msg = JSON.parse(line);
    if (msg.id === undefined) return;
    if (msg.method === "initialize") {
      send(msg.id, {
        protocolVersion: msg.params?.protocolVersion ?? "2025-06-18",
        capabilities: { tools: {} },
        serverInfo: { name: "sidevoice", version: "battery" },
        instructions,
      });
    } else if (msg.method === "tools/list") {
      send(msg.id, { tools });
    } else if (msg.method === "tools/call") {
      const { name, arguments: args } = msg.params;
      let out = canned[name] ?? { ok: true };
      if (name === "voice_say") out = { ok: true, published: true };
      if (name === "run_shell") out = { output: shellOutput(args?.command) };
      send(msg.id, { content: [{ type: "text", text: JSON.stringify(out) }] });
    } else {
      send(msg.id, {});
    }
  });
}
