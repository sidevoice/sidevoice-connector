# Voice adherence battery

Does a model keep the speaking rules in the MCP instructions? No stack: `stub-mcp.mjs` serves the exact `INSTRUCTIONS`
and tool schemas it reads from `connector/src/mcp.rs` (it fails if they change shape), plus a fake `run_shell`;
`run.mjs` plays the scripted Spanish conversations from `scripts.mjs` through the headless `claude` CLI, one process per
conversation, with the real envelope; `report.mjs` scores them and writes a Markdown transcript per model x style x
variant. Needs `claude` logged in and Node 20+. No API key is used; runs spend that login's quota.

    node run.mjs --out target/adherence      # 3 models x 5 envelope variants x scripts a,b,c,e,f,g x 3 reps (resumes)
    node report.mjs --out target/adherence   # measurable signals + Opus judge (cached), table.md and transcripts

Signals are advisory, not a gate. Variants (see `scripts.mjs`): `current` (the envelope as shipped), `mandatory` (a "you must follow
the rules" pointer), `tags` / `afterheader` / `atend` (the same short rule list in the header, after it, or at the end).
Script `f` plays a 31-turn conversation with a real `/compact` in the middle; `g` exercises corrections, risks, plans and the
heads-up before long work. `run.mjs --budget USD` stops starting runs past a spend; `--parallel N` bounds concurrency.
