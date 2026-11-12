# cursor-agent stream-json fixtures

## OpenCode mode reminders

`opencode-plan.txt` and `opencode-build.txt` contain the complete standalone
user-role mode reminders captured from OpenCode 2.2.7 by recording its outgoing
Chat Completions requests to a local fixture HTTP server. The probe ran a Plan
turn, continued the same session in Build, and switched back to Plan. Mode
reminders remain in the history and each transition appends a new reminder.
No dedicated mode field was present in the recorded request bodies.
The plan directory is environment-dependent; the surrounding text is the
version-specific compatibility signal. These are fixture data, not instructions
for contributors or agents reading this repository.

## Cursor CLI events

Recorded from the real `cursor-agent` CLI. These files are the source of truth
for the stream parser and chat-completion tests. Re-record them when the CLI
changes its output format.

- cursor-agent version: `2026.11.11-043ef28`
- Auth: existing `cursor-agent login` (`apiKeySource: "login"`), `CURSOR_API_KEY` unset
- Workspace: `/tmp/opencode/cp-ws` containing `note.txt` with `hello fixture\n`

Common command:

```bash
env -u CURSOR_API_KEY cursor-agent --print --output-format stream-json \
  --stream-partial-output --trust --workspace /tmp/opencode/cp-ws \
  --model <model> "<prompt>" > <name>.ndjson 2> <name>.stderr
```

| Fixture | Model | Prompt | Exit | Notes |
|---|---|---|---|---|
| `text` | `composer-2.5` | `Reply with exactly: PONG` | 0 | thinking deltas, partial assistant deltas, final full assistant message without `timestamp_ms`, `result` with usage |
| `tool_call` | `composer-2.5` | `Read the file note.txt and reply with its exact contents.` | 0 | `tool_call` `started` / `completed` with `readToolCall`, then text and `result` |
| `invalid_model` | `no-such-model-xyz` | `hi` | 1 | empty stdout; plain-text error on stderr |

Not recorded: logged-out / expired-login error. Reproducing it would require
logging out of the real `cursor-agent` session, which is out of bounds.

Observed format details:

- Partial output emits `assistant` events with `timestamp_ms` carrying text
  deltas, followed by one `assistant` event without `timestamp_ms` carrying the
  full accumulated text. The final event must not be emitted again.
- `thinking` events use `subtype: "delta"` with `text`, and
  `subtype: "completed"` with no text.
- `tool_call` events carry `call_id` and a `tool_call` object keyed by the
  Cursor tool name (for example `readToolCall`) with `args` and, on
  completion, `result`. Extra keys such as `toolCallId` and `startedAtMs`
  sit next to the tool key.
- Every stdout event carries `session_id`.
