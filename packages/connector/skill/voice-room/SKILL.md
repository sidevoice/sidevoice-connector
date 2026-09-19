---
name: voice-room
description: Join the user's Sidevoice voice room with this conversation. Use when the user asks to enable voice, join the room or talk by voice; never as a side effect of other work.
argument-hint: "[title for this conversation in the room]"
# Same shape as settings.json hooks (a list of groups): Claude Code 2.1.x registers nothing for a map here.
# The installer replaces __SIDEVOICE_SKILL_DIR__ with the directory it copies this file into: the hook must
# resolve without any variable, since ${CLAUDE_SKILL_DIR} is not expanded in a user skill's hook command.
hooks:
  UserPromptSubmit:
    - hooks:
        - type: command
          command: node "__SIDEVOICE_SKILL_DIR__/hook.mjs"
          timeout: 5
  Stop:
    - hooks:
        - type: command
          command: node "__SIDEVOICE_SKILL_DIR__/hook.mjs"
          timeout: 5
metadata:
  sidevoice: installed copy; the source is skill/voice-room in @sidevoice/uplink, reinstall with `sidevoice skill install`
---

Join the voice room for this conversation and keep it reachable.

1. Call `voice_status`. If it reports `joined` and `room_reachable`, say so in one line and stop.
2. Call `voice_connect` with the title `$ARGUMENTS` when given, otherwise a short label of what this conversation is about.
3. Tell the user in one line whether the room can reach this conversation. If `inbound.ok` is false, relay `inbound.reason` and offer `inbound.remedy` in your own words, including what safeguard it removes; change nothing yourself.

From now on this conversation has read receipts and mechanical working-state reporting: invoking this skill registered hooks that tell the room when a voice message is admitted and when the turn stops. Voice messages arrive as user messages that begin with a JSON header (`channel`, `session_id`, `revision`, `message_id`) followed by the user's words. For each one, publish a short `voice_say` with that `session_id` and `revision` before any other tool, then do the work and publish the result by voice as well. A repeated `message_id` is a redelivery: do not act on it twice.
