/** Shared English fallback messages for legacy JavaScript and native Rust control. */
import agentErrors from "./agent-errors.json" with { type: "json" };
import messages from "./en.json" with { type: "json" };

export const en = { ...agentErrors, ...messages };
