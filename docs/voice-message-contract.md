# Voice-message MCP integration contract

**Status:** Reviewed contract for implementation and validation, 2026-10-17. This describes intended, unshipped behavior; it does not claim the connector implements it. The [full contract](https://github.com/sidevoice/sidevoice-core/pull/127) is canonical. Tracking: [connector #155](https://github.com/sidevoice/sidevoice-connector/issues/155) and [core #126](https://github.com/sidevoice/sidevoice-core/issues/126).

## Connector responsibilities

The initial contract targets push delivery for Codex, Claude Code, and the operator's Cursor bridge. Pull delivery is deferred; when designed, it must preserve the same logical events and stable IDs. Host liveness and wake integration are separate questions.

For inbound room events, render one canonical JSON header followed by a blank line and only the person's exact words for a voice event. A note event is JSON only and contains no fabricated user text. Keep the general reply, retry, recovery, and causal-assent guidance centralized in MCP server instructions and tool descriptions rather than repeating generic prose in every delivered event. Preserve the event's identity, provenance, context, playback-at-turn-start evidence, and hearing changes; unknown evidence remains unknown.

`voice_connect` supplies initial reply context for the active binding. After a room event, calls such as `voice_say` use the latest received context and event ID, including when a native harness initiated the work. Admission returns a prompt accepted or rejected result; accepted means stored under the defined live lifecycle, not queued, playing, or heard. Preserve same-ID retry behavior and bind calls, lookup, and replay to the owning conversation. A transport timeout is indeterminate, so retry with the same IDs.

The planned read and replay tools address an original by ID while its text remains available. Lookup is read-only; replay needs fresh context and current focus/listener checks. Unavailable content is reported as unavailable, without implying that it was heard. The contract requires no durable Core archive or restart recovery; restart invalidates stale bindings and automatic playback eligibility. Keep existing hearing evidence and recovery behavior until a replacement is validated.

On a relevant return or recovery, deliver physical facts as a correlated note to the same bound conversation. A return note does not itself authorize playback: the agent may choose an explicit replay using fresh context. A focus switch before sound must prevent playback to the old conversation. Test actual MCP calls and tool-action traces in supported harnesses, including weaker models; instructions alone cannot guarantee model behavior.

## Separate follow-ups

The [absent-listener optimization](https://github.com/sidevoice/sidevoice-connector/issues/156) and [spoken progress cadence](https://github.com/sidevoice/sidevoice-connector/issues/2) are separate follow-ups, not blockers for this contract. The connector must preserve the current no-listener behavior pending that work.
