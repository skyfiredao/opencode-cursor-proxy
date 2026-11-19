# OpenCode Cursor Proxy

**[中文版](README_zh.md)**

Use Cursor models in OpenCode with streaming replies, tools, automatic connection setup and model-list updates.

This is an independent project, not an official OpenCode or Cursor project. Compatibility with future changes to the official `cursor-agent` is not guaranteed.

## Quick start

### 1. Install and log in to Cursor CLI

Follow the [Cursor CLI installation guide](https://cursor.com/docs/cli/installation): use a shell on macOS, Linux, or WSL, and PowerShell on Windows. On macOS, Linux, and WSL, add `~/.local/bin` to `PATH`.

1. Run `cursor-agent --version` to check the installation.
2. Run `cursor-agent login` and sign in to your Cursor account in the browser.
3. Run `cursor-agent status` to check your login status.

If the installed command is named `agent`, use `agent` above and add `--cursor-agent agent` when starting the proxy. See the [Cursor CLI authentication guide](https://cursor.com/docs/cli/reference/authentication) for login details.

Run the proxy under the same OS user and home directory as Cursor CLI.

### 2. Prepare the proxy

Download your platform's package from a `cursor-proxy-YYYY-MMDD` entry in the repository's Releases: `.tar.gz` for Linux/macOS, `.zip` for Windows.

Supported platforms: Windows amd64, macOS arm64 (macOS 14 or newer), Linux arm64, and Linux amd64.

Debug builds from GitHub Actions contain the executable directly inside the artifact ZIP, with no nested archive. After extracting it on macOS/Linux, run `chmod +x ./cursor-proxy`.

On macOS, if Apple cannot verify the executable is free of malware, confirm its source is trusted before removing its quarantine attribute. Run from the directory containing the executable:

```bash
xattr -d com.apple.quarantine ./cursor-proxy
```

### 3. Start the proxy

Run from the directory containing the executable:

macOS / Linux:

```bash
./cursor-proxy --workspace "$PWD"
```

Windows PowerShell:

```powershell
.\cursor-proxy.exe --workspace .
```

These commands use the current directory as the project workspace. Replace the `--workspace` value with your project's absolute path if needed. If omitted, the workspace defaults to the current directory. Cursor automatically trusts the startup workspace.

The default address is `127.0.0.1:32124`. Keep the terminal open; press `Ctrl-C` to stop the proxy.

### 4. Select a model in OpenCode

1. Open `/models` in OpenCode.
2. Select `cursor-proxy/composer-2.5` or another model under that provider.

The endpoint and model list are configured automatically; no `/connect` step is needed. Run `/reload` if the list has not updated.

## Models and configuration

The proxy fetches models on startup and every hour afterward. Failed refreshes keep the previous result. Unchanged lists do not rewrite configuration.

OpenCode V1 and V2 are supported. Keep the intended `opencode` command on `PATH` for automatic configuration. If version detection fails, the proxy exits with an error. Restart the proxy after switching OpenCode installations.

Before each user turn, the proxy selects the largest context capacity advertised by Cursor and verifies that Cursor accepted it. This capacity replaces an old OpenCode context limit; explicit output limits remain unchanged. If Cursor does not advertise context choices, the existing observation fallback remains in use. In V1, a context limit of `0` means unknown.

Synchronization preserves comments, other providers, the default model, and existing model customizations. It does not delete old models. Existing files receive a `.cursor-proxy-*.bak` backup before changes.

The configuration directory is selected in this order:

1. `OPENCODE_CONFIG_DIR`
2. `$XDG_CONFIG_HOME/opencode`
3. `~/.config/opencode` (default)

An existing `opencode.jsonc` takes priority over `opencode.json`. If neither exists, the proxy creates `opencode.jsonc`.

## Usage options

View options (`-h` also works):

```bash
./cursor-proxy --help
```

On Windows, use `.\cursor-proxy.exe --help`.

`--version` shows `0.2.N+<SHA7>`, where `N` counts commits affecting `cursor-proxy/` since the `0.2` version series was introduced. Rebuilding the same commit does not increment it.

| Option | Purpose | Default |
| --- | --- | --- |
| `--host` | Listen address | `127.0.0.1` |
| `--port` | Listen port | `32124` |
| `--password` | Set an access password; required for access from other computers | Not set |
| `--workspace` | Default project directory | Current directory |
| `--cursor-agent` | Cursor CLI path | `cursor-agent` |
| `--timeout-secs` | Reply timeout in seconds | `300` |
| `--load-skills` | Load project instructions from `AGENTS.md` | Off |
| `--allow-cursor-tools` | Automatically approve Cursor native tools in Build mode | Off |

To change the default project directory, restart the proxy with another `--workspace` value.

## HTTP interface

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/health` | Health check |
| GET | `/v1/models` | Model list |
| POST | `/v1/chat/completions` | Streaming or non-streaming chat |

- The `/v1` prefix can be omitted.
- When `--password` is set, API requests require `Authorization: Bearer <password>`. `/health` remains public.
- Only text input and a single response (`n=1`) are supported. Images, audio, sampling parameters, and token-budget settings are not supported.
- Reasoning is returned as `reasoning_content`; token usage and context estimates are returned in `usage` when available.
- Context estimates may lag. Costs use configured model rates as estimates, not Cursor bills; without rates, costs may not appear.
- For session continuity, send `x-opencode-session-id`, or legacy `X-Session-Id` if the preferred header is absent. Requests without either header are independent.
- API clients can override the default directory with an absolute path in `x-opencode-directory`; UTF-8 percent-encoding is supported. A different directory may require Cursor trust approval.
- Model availability depends on your Cursor account permissions and quota.

## Tools and modes

OpenCode executes client-provided tools and controls their argument validation, permissions and retries. Cursor's native tools may still modify files or run commands directly; OpenCode permissions do not cover those operations.

By default, the proxy rejects approval requests for Cursor native tools. Start with `--allow-cursor-tools` to approve them automatically in Build mode. This permits native file changes and command execution without OpenCode permission checks. The option does not apply in Plan mode or during empty-reply recovery, and does not change OpenCode-managed tool permissions.

Conversations continue across turns and can resume after a proxy restart. If recovery is unavailable, the proxy starts a new session with the context supplied by OpenCode.

Session associations are saved in `cursor-proxy/sessions.db` under the selected OpenCode configuration directory. Records unused for more than 90 days are cleaned automatically, excluding active requests and pending tools. Recovery does not guarantee model-side cache hits.

Switch Plan / Build in OpenCode. Compatibility with other versions or custom agents requires separate verification.

In both Plan and Build, the proxy forwards the declared tools subject to `tool_choice`; OpenCode controls their execution permissions. Plan can inspect Git history through bridged command tools and write plan files when OpenCode permits it.

## Empty-reply recovery

If the upstream turn ends normally with reasoning but no reply text, and no tool activity was observed, the proxy requests a final answer once in the same conversation. It does not replay the original task, switch models automatically, or extend the original request timeout. Another empty reply returns an error.

## License

[GPL-3.0-only](LICENSE).
