---
name: voice-room
description: Join the user's Sidevoice voice room with this conversation. Use when the user asks to enable voice, join the room or talk by voice; never as a side effect of other work.
argument-hint: "[title for this conversation in the room]"
metadata:
  sidevoice: installed copy; the source is skill/voice-room in @sidevoice/uplink, reinstall with `sidevoice skill install`
---

Join the voice room for this conversation and keep it reachable.

1. Call `voice_status`. If it reports `joined` and `room_reachable`, say so in one line and stop.
2. Call `voice_connect` with the title `$ARGUMENTS` when given, otherwise a short label of what this conversation is about. If the user named a room, pass its address as `room`.
   If it fails saying this machine is not paired with the room (or is paired with a different one), ask the user for the room's address and the one-time code the room shows them under **Emparejar conector**; call `voice_pair` with both, then `voice_connect` again. Never try to get a code from the room yourself.
3. Tell the user in one line whether the room can reach this conversation. If `inbound.ok` is false, relay `inbound.reason` and offer `inbound.remedy` in your own words, including what safeguard it removes; change nothing yourself.

Nothing else is registered: the room learns that a message was read and whether this conversation is working from what Claude Code itself records about the session. How to behave once joined is in the Sidevoice MCP server's own instructions.
