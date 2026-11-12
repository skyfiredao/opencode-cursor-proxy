# OpenCode Cursor Proxy

**[中文版](README_zh.md)**

Stream Cursor CLI conversations into OpenCode through ACP, bridge tools through MCP, and configure the connection and model list automatically.

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

These commands use the current directory as the project workspace. Replace the `--workspace` value with your project's absolute path if needed. If omitted, the workspace defaults to the current directory. The proxy always declares the startup workspace trusted to Cursor.

The default address is `127.0.0.1:32124`. Keep the terminal open; press `Ctrl-C` to stop the proxy.

### 4. Select a model in OpenCode

1. Wait for `model catalog refreshed` in the proxy log.
2. Open `/models` in OpenCode.
3. Select `cursor-proxy/composer-2.5` or another model under that provider.

The endpoint and model list are configured automatically; no `/connect` step is needed. Run `/reload` if the list has not updated.

## Models and configuration

The proxy fetches models on startup and every hour afterward. Failed refreshes keep the previous result. Unchanged lists do not rewrite configuration.

OpenCode V1 and V2 are supported. At startup, the proxy checks `opencode --version` from `PATH` and writes the matching configuration format. All `1.x` versions select V1; `2.x` selects V2. If detection fails, it logs a warning and uses V2. When changing the OpenCode installation, restart the proxy so it checks again.

In V1, a context limit of `0` means unknown and can be filled from observed capacity. Positive context limits and explicit output limits are preserved.

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

| Option | Purpose | Default |
| --- | --- | --- |
| `--host` | Listen address | `127.0.0.1` |
| `--port` | Listen port | `32124` |
| `--password` | Set an access password; required for access from other computers | Not set |
| `--workspace` | Project directory | Current directory |
| `--cursor-agent` | Cursor CLI path | `cursor-agent` |
| `--timeout-secs` | Reply timeout in seconds | `300` |
| `--load-skills` | Load project instructions from `AGENTS.md` | Off |

The proxy uses the project directory selected by `--workspace`. To switch projects, stop the proxy and start it with the new directory.

## HTTP interface

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/health` | Health check |
| GET | `/v1/models` | Model list |
| POST | `/v1/chat/completions` | Streaming or non-streaming chat |

- The `/v1` prefix can be omitted.
- Only text input and a single response (`n=1`) are supported. Images, audio, sampling parameters, and token-budget settings are not supported.
- Reasoning is returned as `reasoning_content`. Statistics prefer upstream token breakdowns; otherwise, the current ACP session's used context is mapped to standard `usage` for OpenCode.
- Context fallback uses the latest persisted count and may lag; output and cache breakdowns are not invented. Observed capacity fills missing model limits while preserving existing settings. Costs use configured model rates as estimates, not Cursor bills; without rates, costs may not appear.
- Model availability depends on your Cursor account permissions and quota.

## Tools and modes

OpenCode executes tools forwarded through the MCP bridge. Results return to the original ACP conversation before the reply continues. Cursor's native tools may still modify files or run commands directly; OpenCode permissions do not cover those operations.

Tool-result continuation needs no extra option. After a final reply, the proxy closes the ACP conversation; the next user turn starts a new conversation with OpenCode's supplied history.

The proxy synchronizes Plan / Build using OpenCode's mode reminders. Compatibility with other versions or custom agents requires separate verification.

In Plan mode, the MCP bridge allows only `read`, `glob`, `grep`, and `question`, and no ACP execution approvals are granted for other bridged tools. Writing plan-directory files, running commands, delegating tasks, and custom tools are unsupported; Build is unchanged. The proxy does not sandbox Cursor's native runtime.

## Empty-reply recovery

If the upstream turn ends normally with reasoning but no reply text, and no tool activity was observed, the proxy requests a final answer once in the same conversation. It does not replay the original task, switch models automatically, or extend the original request timeout. Another empty reply returns an error.

See [DESIGN.md](DESIGN.md) for architecture, thread continuation, and recovery boundaries.

## License

[GPL-3.0-only](LICENSE).
