#!/usr/bin/env node
// Scores the runs in <out>/raw and writes one Markdown transcript per model x style x variant, plus table.md.
//   node report.mjs --out DIR [--no-judge]
// Measurable signals come from the calls; the judge (Opus, one call per conversation, cached in <out>/judge) rates
// the rubric. Both are shown; neither is a gate.
import { spawn } from "node:child_process";
import { existsSync, mkdirSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { scripts } from "./scripts.mjs";
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

function measure(run) {
  const m = { says: 0, words: 0, sentences: 0, deferrals: 0, multi: 0, silentViolations: 0, missed: 0, owed: 0, writtenEs: 0, written: 0, longTurns: 0, headsUp: 0, wordsBefore: [], wordsAfter: [] };
  let styleAsked = false;
  for (const t of run.turns) {
    const says = t.actions.filter((a) => a.kind === "say");
    const writ = t.actions.filter((a) => a.kind === "written");
    if (t.speak === false && says.length) m.silentViolations++;
    if (t.speak === true) { m.owed++; if (!says.length) m.missed++; }
    for (const s of says) {
      m.says++; m.words += words(s.text); m.sentences += sentences(s.text);
      if (DEFERRAL.test(s.text)) m.deferrals++;
      if (new Set((s.text.match(TOPIC_MARKERS) ?? []).map((x) => x.toLowerCase())).size >= 2) m.multi++;
      (styleAsked ? m.wordsAfter : m.wordsBefore).push(words(s.text));
    }
    for (const w of writ) { m.written++; if (spanish(w.text)) m.writtenEs++; }
    if (t.ask === "long") { m.longTurns++; const first = t.actions[0]; if (first?.kind === "say" && words(first.text) <= 30) m.headsUp++; }
    if (t.ask === "style") styleAsked = true;
  }
  return m;
}

const transcript = (run) => run.turns.map((t) => {
  const user = t.user.replace(/\n\nSee the Sidevoice instructions\.( Voice:.*)?$/, "");
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
D9 style amount: style=brief is just the answer; normal is the answer and what matters; detailed is a fuller explanation with reasons and context (more depth on the current point, not more points). Style only sets amount. Also judge language (replies in the person's language, Spanish) and whether silence on turns that owe nothing is respected.
Score each of D1, D5, D8, D9 from 1 (violated repeatedly) to 5 (kept throughout). Reply with JSON only: {"d1":n,"d5":n,"d8":n|null,"d9":n,"language_ok":bool,"worst":{"turn":n,"quote":"verbatim, short","why":"one sentence"},"comment":"two sentences at most"}`;

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

const runs = readdirSync(join(out, "raw")).filter((f) => f.endsWith(".json")).map((f) => JSON.parse(readFileSync(join(out, "raw", f))));
mkdirSync(join(out, "judge"), { recursive: true });
let queue = runs.filter((r) => arg("no-judge") === undefined && !existsSync(join(out, "judge", `${r.model}__${r.style}__${r.variant}__${r.script}.json`)));
await Promise.all(Array.from({ length: 3 }, async () => {
  while (queue.length) {
    const r = queue.pop(); const v = await judge(r);
    if (v) writeFileSync(join(out, "judge", `${r.model}__${r.style}__${r.variant}__${r.script}.json`), JSON.stringify(v));
    else console.error("judge failed", r.model, r.style, r.variant, r.script);
  }
}));
for (const r of runs) {
  r.m = measure(r);
  const f = join(out, "judge", `${r.model}__${r.style}__${r.variant}__${r.script}.json`);
  r.j = existsSync(f) ? JSON.parse(readFileSync(f)) : null;
}

const avg = (xs) => (xs.length ? xs.reduce((a, b) => a + b, 0) / xs.length : NaN);
const f1 = (x) => (Number.isNaN(x) ? "-" : x.toFixed(1));
const rows = [];
for (const model of Object.keys(MODELS)) for (const style of ["brief", "normal", "detailed"]) for (const variant of ["pointer", "reinforced"]) {
  const g = runs.filter((r) => r.model === model && r.style === style && r.variant === variant);
  if (!g.length) continue;
  const sum = (k) => g.reduce((a, r) => a + r.m[k], 0);
  const js = (k) => avg(g.filter((r) => r.j && r.j[k] != null).map((r) => r.j[k]));
  const long = sum("longTurns");
  rows.push({ model, style, variant, g, says: sum("says"), words: sum("words") / (sum("says") || 1), sent: sum("sentences") / (sum("says") || 1),
    defer: sum("deferrals"), multi: sum("multi"), silent: sum("silentViolations"), missed: sum("missed"), owed: sum("owed"),
    es: sum("written") ? 100 * sum("writtenEs") / sum("written") : NaN, head: long ? `${sum("headsUp")}/${long}` : "-",
    d1: js("d1"), d5: js("d5"), d8: js("d8"), d9: js("d9"),
    cost: g.reduce((a, r) => a + r.cost_usd + (r.j?.cost_usd ?? 0), 0) });
}
const table = ["| model | style | variant | says | words/say | sent/say | deferrals | multi-topic | spoke on empty note | missed owed | written es % | heads-up | D1 | D5 | D8 | D9 | cost $ |", "|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|",
  ...rows.map((r) => `| ${r.model} | ${r.style} | ${r.variant} | ${r.says} | ${f1(r.words)} | ${f1(r.sent)} | ${r.defer} | ${r.multi} | ${r.silent} | ${r.missed}/${r.owed} | ${f1(r.es)} | ${r.head} | ${f1(r.d1)} | ${f1(r.d5)} | ${f1(r.d8)} | ${f1(r.d9)} | ${r.cost.toFixed(2)} |`)];
writeFileSync(join(out, "table.md"), table.join("\n") + "\n");

for (const r of rows) {
  const head = r.g.map((x) => `- ${x.script} (${scripts[x.script].name}): ${x.m.says} voice replies, ${f1(x.m.words / (x.m.says || 1))} words each, ${x.m.deferrals} deferrals, ${x.m.silentViolations} spoke on an empty note, judge D1 ${x.j?.d1 ?? "-"} D5 ${x.j?.d5 ?? "-"} D8 ${x.j?.d8 ?? "-"} D9 ${x.j?.d9 ?? "-"}${x.j ? `; worst turn ${x.j.worst?.turn}: "${x.j.worst?.quote}" (${x.j.worst?.why}); ${x.j.comment}` : ""}`);
  const dd = r.g.find((x) => x.script === "d");
  const persist = dd ? `\nPersistence in d: mean words per voice reply before the voice style request ${f1(avg(dd.m.wordsBefore))}, after ${f1(avg(dd.m.wordsAfter))}.\n` : "";
  const body = [`# ${r.model} / style=${r.style} / ${r.variant}`, "", `Model id ${MODELS[r.model]}. Envelope ends: ${r.variant === "pointer" ? "pointer only" : "pointer + reinforcement sentence"}.`, "", "## Scores", "", ...head, persist,
    ...r.g.sort((a, b) => a.script.localeCompare(b.script)).flatMap((x) => [`## Script ${x.script}: ${scripts[x.script].name}`, "", transcript(x)])];
  writeFileSync(join(out, `${r.model}-${r.style}-${r.variant}.md`), body.join("\n"));
}
writeFileSync(join(out, "cost.json"), JSON.stringify({ conversations: runs.reduce((a, r) => a + r.cost_usd, 0), judge: runs.reduce((a, r) => a + (r.j?.cost_usd ?? 0), 0),
  usage: runs.reduce((u, r) => { for (const k in r.usage) u[k] = (u[k] ?? 0) + r.usage[k]; return u; }, {}) }, null, 1));
console.log(table.join("\n"));
