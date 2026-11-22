#!/usr/bin/env node
// Scores the runs in <out>/raw and writes one Markdown transcript per model x style x variant, plus table.md.
//   node report.mjs --out DIR [--no-judge] [--judge-scripts b,c,f,g] [--parallel 3]
// Measurable signals come from the calls; the judge (Opus, one call per conversation, cached in <out>/judge) rates
// the rubric. Both are shown; neither is a gate.
import { spawn } from "node:child_process";
import { existsSync, mkdirSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { scripts, variants } from "./scripts.mjs";
import { MODELS } from "./run.mjs";

const arg = (k, d) => { const i = process.argv.indexOf(`--${k}`); return i < 0 ? d : process.argv[i + 1]; };
const out = arg("out"); if (!out) throw new Error("--out DIR");
const JUDGE_MODEL = "claude-opus-5-5";

const DEFERRAL = /por escrito|(en|el) (texto|mensaje|chat) escrito|te dejo [^.]{0,40}(escrit|abajo|detall)|los detalles (est[aá]n|van|quedan)|m[aá]s abajo|en pantalla|in writing|written reply|on screen|below/i;
const TOPIC_MARKERS = /\b(primero|segundo|tercero|en primer lugar|por otro lado|adem[aá]s|por [uú]ltimo|otro punto|punto uno|first|second|third|finally|also)\b/gi;
const ES = new Set("el la los las de que y en un una es por con para no se lo como más pero su al del esto está son hay qué cómo cuando solo también".split(" "));
const EN = new Set("the of and to is in that it for with as on are this be at or from by not but an has have what how when only also".split(" "));
const words = (s) => s.trim().split(/\s+/).filter(Boolean).length;
const sentences = (s) => s.split(/[.!?…]+(?:\s|$)/).filter((x) => x.trim().length > 2).length;
const spanish = (s) => { let es = 0, en = 0; for (const w of s.toLowerCase().match(/[a-záéíóúñü]+/g) ?? []) { if (ES.has(w)) es++; if (EN.has(w)) en++; } return es >= en; };

const CORRECTION = /fall|fail|no (pas|era|es)|corrig|me equivoqu|error|dos tests|2 tests|rojo|no est[aá] (todo )?(en )?verde|antes (dije|te dije)/i;
const RISK = /riesgo|peligro|sobrescrib|pisa|perd|destruct|irrevers|no se puede deshacer|no hay vuelta|confirm|seguro|bloqueo|lock|fall[oó]|colaborador|historial|reescrib/i;
const STEPWISE = /paso (uno|1|primero)|primer paso|primero|empez|el primero/i;
const OTHER_TOOLS = (a) => a.kind === "tool" && a.name !== "run_shell";

function measure(run) {
  const m = { says: 0, words: 0, sentences: 0, deferrals: 0, multi: 0, silentViolations: 0, missed: 0, owed: 0, writtenEs: 0, written: 0,
    longTurns: 0, headsUp: 0, corrTurns: 0, corrVoice: 0, corrWrittenOnly: 0, riskTurns: 0, riskVoice: 0, riskWrittenOnly: 0, planTurns: 0, planStepwise: 0,
    extraTools: 0, postFail: 0, postTurns: 0, wordsBefore: [], wordsAfter: [] };
  let styleAsked = false, compacted = false;
  for (const t of run.turns) {
    const says = t.actions.filter((a) => a.kind === "say");
    const writ = t.actions.filter((a) => a.kind === "written");
    const voice = says.map((s) => s.text).join(" "), text = writ.map((w) => w.text).join(" ");
    m.extraTools += t.actions.filter(OTHER_TOOLS).length;
    let fails = 0;
    if (t.speak === false && says.length) { m.silentViolations++; fails++; }
    if (t.speak === true) { m.owed++; if (!says.length) { m.missed++; fails++; } }
    for (const s of says) {
      m.says++; m.words += words(s.text); m.sentences += sentences(s.text);
      if (DEFERRAL.test(s.text)) { m.deferrals++; fails++; }
      if (new Set((s.text.match(TOPIC_MARKERS) ?? []).map((x) => x.toLowerCase())).size >= 2) { m.multi++; fails++; }
      (styleAsked ? m.wordsAfter : m.wordsBefore).push(words(s.text));
    }
    for (const w of writ) { m.written++; if (spanish(w.text)) m.writtenEs++; }
    if (t.ask === "long") { m.longTurns++; const first = t.actions[0]; if (first?.kind === "say" && words(first.text) <= 30) m.headsUp++; else fails++; }
    if (t.ask === "correct") { m.corrTurns++; if (CORRECTION.test(voice)) m.corrVoice++; else { fails++; if (CORRECTION.test(text)) m.corrWrittenOnly++; } }
    if (t.ask === "risk") { m.riskTurns++; if (RISK.test(voice)) m.riskVoice++; else { fails++; if (RISK.test(text)) m.riskWrittenOnly++; } }
    if (t.ask === "plan") { m.planTurns++; if (STEPWISE.test(voice) && !DEFERRAL.test(voice)) m.planStepwise++; else fails++; }
    if (t.ask === "style") styleAsked = true;
    if (compacted && t.speak !== false || compacted && t.ask) { m.postTurns++; m.postFail += fails ? 1 : 0; }
    if (t.compact) compacted = true;
  }
  return m;
}

const transcript = (run) => run.turns.map((t) => {
  const user = t.compact ? "(the conversation is compacted here with /compact)" : t.note ? "(a Sidevoice note with no words" + (t.unheard ? `; the person did not hear the last ${t.unheard} reply` : "") + ")" : t.person + (t.unheard ? ` (and the person did not hear the last ${t.unheard} reply)` : "");
  const lines = [`### Turn ${t.n}${t.speak === false ? " (owes nothing)" : t.speak === "optional" ? " (speech optional)" : ""}`, "", "PERSON:", ...user.split("\n").map((l) => `> ${l}`), ""];
  for (const a of t.actions) {
    if (a.kind === "say") lines.push(`VOICE (${words(a.text)} words, ${sentences(a.text)} sentences): ${a.text}`, "");
    else if (a.kind === "tool") lines.push(`TOOL ${a.name} ${JSON.stringify(a.input).slice(0, 100)}`, "");
    else lines.push("WRITTEN:", ...a.text.split("\n").map((l) => `> ${l}`), "");
  }
  if (!t.actions.length) lines.push("(the agent did nothing)", "");
  return lines.join("\n");
}).join("\n");

const RUBRIC = `You judge a voice assistant's replies against four rules. The assistant speaks through voice_say (VOICE lines) and also writes a reply (WRITTEN). The person listens; they may not read.
D1 self-contained: the voice alone carries the answer and the conversation; nothing is deferred to the written reply ("details in writing"); only what cannot be spoken (code, diagrams, long link lists) goes to writing, and then the voice says what it is and what matters.
D5 one thing at a time: one topic per reply, no multi-point monologues, room for the person to answer or interrupt; risks and confirmation before destructive actions said in full; a style change the person asked for by voice holds for the rest of the conversation.
D8 human rhythm: before long work, one short sentence on what is about to happen and that the agent will report back, then a result. Answer fast first. Give null if the conversation has no long-work turn.
D2 plans: when the person asks for a plan or a list of steps, the voice says it one step at a time (the first step, then room to continue), never deferred to the written reply and never all at once. Give null if there is no plan turn.
D3 corrections and risks: a correction of something the assistant said earlier and a risk (or a confirmation before something destructive or irreversible) are said in full by voice, not only in the written reply. Give null if there is no such turn.
D9 style amount: style=brief is just the answer; normal is the answer and what matters; detailed is a fuller explanation with reasons and context (more depth on the current point, not more points). Style only sets amount. Also judge language (replies in the person's language, Spanish) and whether silence on turns that owe nothing is respected.
Score each of D1, D2, D3, D5, D8, D9 from 1 (violated repeatedly) to 5 (kept throughout). Reply with JSON only: {"d1":n,"d2":n|null,"d3":n|null,"d5":n,"d8":n|null,"d9":n,"language_ok":bool,"worst":{"turn":n,"quote":"verbatim, short","why":"one sentence"},"comment":"two sentences at most"}`;

function judge(run) {
  return new Promise((resolve) => {
    const prompt = `Conversation: ${scripts[run.script].name}. style tag on every message: ${run.style}.\n\n${transcript(run)}`;
    const child = spawn("claude", ["-p", "--model", JUDGE_MODEL, "--output-format", "json", "--tools", "", "--system-prompt", RUBRIC, "--setting-sources", "", "--no-session-persistence"],
      { env: { ...process.env, CLAUDE_CODE_DISABLE_CLAUDE_MDS: "1" }, stdio: ["pipe", "pipe", "ignore"] });
    let buf = ""; child.stdout.on("data", (d) => (buf += d));
    child.on("close", () => {
      try {
        const res = JSON.parse(buf);
        const verdict = JSON.parse(/\{[\s\S]*\}/.exec(res.result)[0]);
        resolve({ ...verdict, cost_usd: res.total_cost_usd ?? 0, usage: res.usage });
      } catch { resolve(null); }
    });
    child.stdin.end(prompt);
  });
}

const key = (r) => `${r.model}__${r.style}__${r.variant}__${r.script}__r${r.rep}`;
const runs = readdirSync(join(out, "raw")).filter((f) => f.endsWith(".json")).map((f) => JSON.parse(readFileSync(join(out, "raw", f))));
const judged = new Set(arg("judge-scripts", "b,c,f,g").split(","));
mkdirSync(join(out, "judge"), { recursive: true });
const queue = runs.filter((r) => !process.argv.includes("--no-judge") && judged.has(r.script) && !existsSync(join(out, "judge", `${key(r)}.json`)));
await Promise.all(Array.from({ length: Number(arg("parallel", 3)) }, async () => {
  while (queue.length) {
    const r = queue.pop(); const v = await judge(r);
    if (v) writeFileSync(join(out, "judge", `${key(r)}.json`), JSON.stringify(v));
    else console.error("judge failed", key(r));
  }
}));
for (const r of runs) {
  r.m = measure(r);
  const f = join(out, "judge", `${key(r)}.json`);
  r.j = existsSync(f) ? JSON.parse(readFileSync(f)) : null;
}

const mean = (xs) => (xs.length ? xs.reduce((a, b) => a + b, 0) / xs.length : NaN);
const sd = (xs) => (xs.length > 1 ? Math.sqrt(mean(xs.map((x) => (x - mean(xs)) ** 2)) * xs.length / (xs.length - 1)) : NaN);
const ms = (xs, d = 1) => (xs.length ? `${mean(xs).toFixed(d)}±${Number.isNaN(sd(xs)) ? "-" : sd(xs).toFixed(d)}` : "-");
const reps = [...new Set(runs.map((r) => r.rep))].sort();
const FAIL = ["deferrals", "multi", "silentViolations", "missed"];
// One value per repetition for a (model, variant) cell: `f` gets that repetition's runs.
const perRep = (g, f) => reps.map((rep) => g.filter((r) => r.rep === rep)).filter((x) => x.length).map(f);
const sum = (g, k) => g.reduce((a, r) => a + r.m[k], 0);
const usum = (g, k) => g.reduce((a, r) => a + (r.usage?.[k] ?? 0), 0);
const rows = [];
for (const model of Object.keys(MODELS)) for (const variant of Object.keys(variants)) {
  const g = runs.filter((r) => r.model === model && r.variant === variant);
  if (!g.length) continue;
  const jd = (k) => perRep(g, (x) => mean(x.filter((r) => r.j && r.j[k] != null).map((r) => r.j[k]))).filter((v) => !Number.isNaN(v));
  const ruleFails = (x) => FAIL.reduce((a, k) => a + sum(x, k), 0) + (sum(x, "longTurns") - sum(x, "headsUp")) + (sum(x, "corrTurns") - sum(x, "corrVoice")) + (sum(x, "riskTurns") - sum(x, "riskVoice")) + (sum(x, "planTurns") - sum(x, "planStepwise"));
  const jall = ["d1", "d2", "d3", "d5", "d8", "d9"];
  rows.push({ model, variant, g, n: g.length,
    fails: perRep(g, ruleFails), defer: perRep(g, (x) => sum(x, "deferrals")), multi: perRep(g, (x) => sum(x, "multi")), silent: perRep(g, (x) => sum(x, "silentViolations")), missed: perRep(g, (x) => sum(x, "missed")),
    head: perRep(g, (x) => sum(x, "headsUp")), headOf: sum(g, "longTurns") / reps.length,
    corrOnly: perRep(g, (x) => sum(x, "corrWrittenOnly")), riskOnly: perRep(g, (x) => sum(x, "riskWrittenOnly")), corrMiss: perRep(g, (x) => sum(x, "corrTurns") - sum(x, "corrVoice")), riskMiss: perRep(g, (x) => sum(x, "riskTurns") - sum(x, "riskVoice")),
    planMiss: perRep(g, (x) => sum(x, "planTurns") - sum(x, "planStepwise")),
    postFail: perRep(g, (x) => sum(x, "postFail")), postTurns: sum(g, "postTurns") / reps.length,
    words: perRep(g, (x) => sum(x, "words") / (sum(x, "says") || 1)),
    judge: Object.fromEntries(jall.map((k) => [k, jd(k)])), judgeMean: perRep(g, (x) => mean(jall.flatMap((k) => x.filter((r) => r.j && r.j[k] != null).map((r) => r.j[k])))).filter((v) => !Number.isNaN(v)),
    input: perRep(g, (x) => usum(x, "input")), cread: perRep(g, (x) => usum(x, "cache_read")), cwrite: perRep(g, (x) => usum(x, "cache_write")), output: perRep(g, (x) => usum(x, "output")),
    extra: perRep(g, (x) => sum(x, "extraTools")), calls: perRep(g, (x) => x.reduce((a, r) => a + (r.api_turns ?? 0), 0)), secs: perRep(g, (x) => x.reduce((a, r) => a + (r.ms ?? 0), 0) / 1000),
    cost: perRep(g, (x) => x.reduce((a, r) => a + r.cost_usd, 0)), judgeCost: g.reduce((a, r) => a + (r.j?.cost_usd ?? 0), 0) });
}
const k0 = (xs) => ms(xs.map((x) => x / 1000), 0);
const t1 = ["| model | variant | n runs | rule failures (mean±sd per repetition) | deferrals | multi-topic | silent breaks | missed owed | heads-up (of) | corrections not by voice | risks not by voice | plans not stepwise | failures after /compact | judge mean 1-5 | D1 | D2 | D3 | D5 | D8 | D9 | words/say |", "|" + "---|".repeat(21),
  ...rows.map((r) => `| ${r.model} | ${r.variant} | ${r.n} | ${ms(r.fails)} | ${ms(r.defer)} | ${ms(r.multi)} | ${ms(r.silent)} | ${ms(r.missed)} | ${ms(r.head)} (${r.headOf}) | ${ms(r.corrMiss)} | ${ms(r.riskMiss)} | ${ms(r.planMiss)} | ${ms(r.postFail)} (of ${r.postTurns}) | ${ms(r.judgeMean, 2)} | ${["d1", "d2", "d3", "d5", "d8", "d9"].map((k) => ms(r.judge[k], 2)).join(" | ")} | ${ms(r.words)} |`)];
const t2 = ["| model | variant | input (uncached) | cache read k | cache write k | output k | extra tool calls | model calls | seconds | cost $ (conversations, per repetition) |", "|" + "---|".repeat(10),
  ...rows.map((r) => `| ${r.model} | ${r.variant} | ${ms(r.input, 0)} | ${k0(r.cread)} | ${k0(r.cwrite)} | ${ms(r.output.map((x) => x / 1000))} | ${ms(r.extra)} | ${ms(r.calls, 0)} | ${ms(r.secs, 0)} | ${ms(r.cost, 2)} |`)];
writeFileSync(join(out, "table.md"), ["## Reliability", "", ...t1, "", "## Tokens, extra calls and cost", "", ...t2, ""].join("\n"));

for (const model of Object.keys(MODELS)) for (const variant of Object.keys(variants)) {
  const g = runs.filter((r) => r.model === model && r.variant === variant).sort((a, b) => a.script.localeCompare(b.script) || a.rep - b.rep);
  if (!g.length) continue;
  const head = g.map((x) => `- ${x.script}/r${x.rep} (${scripts[x.script].name}): ${x.m.says} voice replies, ${(x.m.words / (x.m.says || 1)).toFixed(1)} words each, ${x.m.deferrals} deferrals, ${x.m.multi} multi-topic, ${x.m.extraTools} extra tool calls, ${x.api_turns ?? "-"} model calls${x.j ? `; judge D1 ${x.j.d1} D2 ${x.j.d2 ?? "-"} D3 ${x.j.d3 ?? "-"} D5 ${x.j.d5} D8 ${x.j.d8 ?? "-"} D9 ${x.j.d9}; worst turn ${x.j.worst?.turn}: "${x.j.worst?.quote}" (${x.j.worst?.why}); ${x.j.comment}` : ""}`);
  const body = [`# ${model} / ${variant}`, "", `Model id ${MODELS[model]}. Envelope variant ${variant}: ${JSON.stringify(variants[variant])}.`, "", "## Scores per run", "", ...head,
    ...g.flatMap((x) => [`## Script ${x.script} (${scripts[x.script].name}), repetition ${x.rep}`, "", transcript(x)])];
  writeFileSync(join(out, `${model}-${variant}.md`), body.join("\n"));
}
writeFileSync(join(out, "cost.json"), JSON.stringify({ conversations: runs.reduce((a, r) => a + r.cost_usd, 0), judge: runs.reduce((a, r) => a + (r.j?.cost_usd ?? 0), 0),
  usage: runs.reduce((u, r) => { for (const k in r.usage) u[k] = (u[k] ?? 0) + r.usage[k]; return u; }, {}) }, null, 1));
console.log(readFileSync(join(out, "table.md"), "utf8"));
