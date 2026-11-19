# OpenCode Cursor Proxy

**[English](README.md)**

在 OpenCode 中使用 Cursor 模型，支持流式回复、工具调用、自动配置连接和更新模型列表。

本项目为独立项目，并非 OpenCode 或 Cursor 官方项目，不保证始终兼容官方 `cursor-agent` 的后续变更。

## 快速开始

### 1. 安装并登录 Cursor CLI

按 [Cursor CLI 安装指南](https://cursor.com/docs/cli/installation) 安装：macOS、Linux 和 WSL 使用 Shell，Windows 使用 PowerShell。macOS、Linux 和 WSL 需将 `~/.local/bin` 加入 `PATH`。

1. 运行 `cursor-agent --version`，确认安装成功。
2. 运行 `cursor-agent login`，在浏览器中登录 Cursor 账号。
3. 运行 `cursor-agent status`，确认登录状态。

如果安装后的命令名为 `agent`，上述命令改用 `agent`，启动代理时加上 `--cursor-agent agent`。登录步骤详见 [Cursor CLI 认证指南](https://cursor.com/docs/cli/reference/authentication)。

代理与 Cursor CLI 使用相同系统用户和用户主目录。

### 2. 准备代理

从仓库 Releases 中的 `cursor-proxy-YYYY-MMDD` 条目下载对应平台的包：Linux/macOS 为 `.tar.gz`，Windows 为 `.zip`。

支持 Windows amd64、macOS arm64（macOS 14 及以上）、Linux arm64 和 amd64。

GitHub Actions 的 Debug 产物 ZIP 内直接包含可执行文件，不再嵌套压缩包。macOS/Linux 解压后运行 `chmod +x ./cursor-proxy`。

macOS 若提示 Apple 无法验证该文件是否包含恶意软件，请先确认文件来源可信，再移除该文件的下载隔离标记。在可执行文件所在目录运行：

```bash
xattr -d com.apple.quarantine ./cursor-proxy
```

### 3. 启动代理

在可执行文件所在目录运行：

macOS / Linux：

```bash
./cursor-proxy --workspace "$PWD"
```

Windows PowerShell：

```powershell
.\cursor-proxy.exe --workspace .
```

上述命令将当前目录作为项目目录，也可将 `--workspace` 的值改为目标项目的绝对路径。省略时默认使用当前目录。Cursor 会自动信任启动时选定的目录。

默认监听 `127.0.0.1:32124`。保持终端运行，按 `Ctrl-C` 停止代理。

### 4. 在 OpenCode 选择模型

1. 在 OpenCode 中打开 `/models`。
2. 选择 `cursor-proxy/composer-2.5` 或该 provider 下的其他模型。

连接地址和模型列表自动写入配置，无需执行 `/connect`。列表未更新时执行 `/reload`。

## 模型与配置

代理启动时获取模型列表，此后每小时刷新一次。刷新失败时保留上次结果，列表未变化时不重写配置。

支持 OpenCode V1 和 V2。请将要使用的 `opencode` 命令放在 `PATH` 中，以便自动配置。版本识别失败时代理报错退出。切换 OpenCode 安装后需重启代理。

每轮用户对话前，代理选择 Cursor 公布的最大上下文容量，并核对上游已接受。该容量会更新 OpenCode 中的旧上下文限制，显式输出限制保持不变。上游未公布容量选项时，沿用已有的观测回退。V1 的上下文限制为 `0` 时表示未知。

同步保留注释、其他 provider、默认模型及已有模型的自定义设置，不自动删除旧模型。修改已有文件前生成 `.cursor-proxy-*.bak` 备份。

配置目录按以下顺序选择：

1. `OPENCODE_CONFIG_DIR`
2. `$XDG_CONFIG_HOME/opencode`
3. `~/.config/opencode`（默认）

优先更新目录内已有的 `opencode.jsonc`，其次为 `opencode.json`；均不存在时创建 `opencode.jsonc`。

## 使用选项

查看参数（也可用 `-h`）：

```bash
./cursor-proxy --help
```

Windows 使用 `.\cursor-proxy.exe --help`。

`--version` 显示 `0.2.N+<SHA 前 7 位>`，`N` 为引入 `0.2` 版本系列后影响 `cursor-proxy/` 的提交数。同一提交重复构建不递增。

| 参数 | 用途 | 默认值 |
| --- | --- | --- |
| `--host` | 监听地址 | `127.0.0.1` |
| `--port` | 监听端口 | `32124` |
| `--password` | 设置访问密码，允许其他电脑访问时必填 | 未设置 |
| `--workspace` | 默认项目目录 | 当前目录 |
| `--cursor-agent` | Cursor CLI 路径 | `cursor-agent` |
| `--timeout-secs` | 回复超时秒数 | `300` |
| `--load-skills` | 加载 `AGENTS.md` 中的项目指令 | 关闭 |
| `--allow-cursor-tools` | 在 Build 模式下自动批准 Cursor 原生工具 | 关闭 |

修改默认项目目录时，用新的 `--workspace` 值重启代理。

## HTTP 接口

| 方法 | 路径 | 用途 |
| --- | --- | --- |
| GET | `/health` | 健康检查 |
| GET | `/v1/models` | 模型列表 |
| POST | `/v1/chat/completions` | 流式或非流式聊天 |

- 也支持省略 `/v1` 前缀。
- 设置 `--password` 后，API 请求需携带 `Authorization: Bearer <password>`；`/health` 无需认证。
- 仅支持文本输入和单条回复（`n=1`），暂不支持图片、音频、采样参数和 token 预算设置。
- 思考内容通过 `reasoning_content` 返回；有统计数据时，通过 `usage` 返回 token 用量和上下文估计。
- 上下文估计可能滞后。费用按已有模型单价估算，不代表 Cursor 实际账单；未配置单价时可能不显示费用。
- 需要延续会话时，发送 `x-opencode-session-id`；没有该头时兼容旧版 `X-Session-Id`。两者均未提供时，请求独立处理。
- API 客户端可通过 `x-opencode-directory` 提供绝对路径，覆盖默认目录，支持 UTF-8 百分号编码。其他目录可能需要 Cursor 信任确认。
- 模型实际可用性取决于 Cursor 账号权限和额度。

## 工具与模式

OpenCode 执行客户端提供的工具，并控制参数校验、执行权限和重试。Cursor 原生工具仍可能直接修改文件或执行命令，OpenCode 权限不覆盖这些操作。

代理默认拒绝 Cursor 原生工具的审批请求。启动时添加 `--allow-cursor-tools`，可在 Build 模式下自动批准原生工具请求，允许原生文件修改和命令执行绕过 OpenCode 权限检查。该选项不适用于 Plan 模式和空回复恢复，也不改变 OpenCode 管理的工具权限。

对话可以跨轮次延续，并在代理重启后恢复。无法恢复时，代理使用 OpenCode 提供的上下文创建新会话。

会话关联数据保存在所选 OpenCode 配置目录下的 `cursor-proxy/sessions.db`。超过 90 天未使用的记录会自动清理，跳过正在处理请求和等待工具结果的会话。会话恢复不保证模型端缓存命中。

在 OpenCode 中切换 Plan / Build 即可；其他版本或自定义 agent 的兼容性需另行验证。

Plan 和 Build 模式均按 `tool_choice` 转发声明的工具，由 OpenCode 控制执行权限。OpenCode 允许时，Plan 可通过桥接命令工具查看 Git 历史，也可写入计划文件。

## 空回复恢复

上游正常结束但只有思考、没有正文，且未观测到工具活动时，代理在原对话中最多请求一次最终回答。不会重放原任务、自动切换模型或延长原请求超时；补答仍为空则报错。

## 许可

[GPL-3.0-only](LICENSE)。
