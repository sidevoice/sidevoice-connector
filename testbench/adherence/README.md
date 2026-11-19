# Voice adherence battery

Does a model keep the speaking rules in the MCP instructions? No stack: `stub-mcp.mjs` serves the exact `INSTRUCTIONS`
and tool schemas it reads from `connector/src/mcp.rs` (it fails if they change shape), plus a fake `run_shell`;
`run.mjs` plays the scripted Spanish conversations from `scripts.mjs` through the headless `claude` CLI, one process per
conversation, with the real envelope; `report.mjs` scores them and writes a Markdown transcript per model x style x
variant. Needs `claude` logged in and Node 20+. No API key is used; runs spend that login's quota.

    node run.mjs --out target/adherence      # 3 models x 3 styles x 2 envelope variants x 5 scripts (resumes)
    node report.mjs --out target/adherence   # measurable signals + Opus judge (cached), table.md and transcripts

Signals are advisory, not a gate. Variants: `pointer` (the envelope as shipped) and `reinforced` (plus one sentence).
