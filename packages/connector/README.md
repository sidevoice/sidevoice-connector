# Sidevoice connector

This is the local half of the production-oriented transport. It deliberately
does not expose an inbound port to the internet.

`connector.mjs` owns one outbound WebSocket per host and multiplexes the active
room bindings over it. `mcp.mjs` is a short-lived stdio MCP façade: it starts or
reuses the connector and sends it local IPC commands. The connector exits after
the last binding is removed (with a short grace period).

The control-plane WebSocket contract is newline-free JSON messages:

- client -> server: `connector.hello`, `binding.register`, `binding.unregister`,
  `speech.publish`, `input.ack`, and `heartbeat`;
- server -> client: `input.deliver` and `heartbeat.ack`.

Every delivery has an `event_id`. The connector preserves it when posting to the
harness adapter and returns an acknowledgement only after that adapter confirms
acceptance. On reconnect it re-sends `connector.hello` and every active binding.
The remote control plane is responsible for durable events and replay after the
last acknowledged event.

## Local development

The code uses Node 22's built-in `WebSocket`, `fetch`, stdio and Unix-domain
sockets; there are no npm dependencies.

```sh
export SIDEVOICE_CONTROL_URL=wss://sidevoice.example/connectors
export SIDEVOICE_CONNECTOR_TOKEN=development-token
node connector/mcp.mjs
```

The checked-in voice-room prototype is still local-only and does not implement
this control-plane endpoint. The connector is therefore a new integration layer,
not a claim that the current `127.0.0.1:8767` server is remotely deployable.
