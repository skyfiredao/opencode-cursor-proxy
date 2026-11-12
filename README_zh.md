# OpenCode Cursor Proxy

**[English](README.md)**

通过 Cursor CLI 的 ACP 接口为 OpenCode 提供流式对话，使用 MCP 桥接工具，自动配置连接并同步模型列表。

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

上述命令将当前目录作为项目目录，也可将 `--workspace` 的值改为目标项目的绝对路径。省略时默认使用当前目录。代理始终向 Cursor 声明启动时选定的工作目录可信。

默认监听 `127.0.0.1:32124`。保持终端运行，按 `Ctrl-C` 停止代理。

### 4. 在 OpenCode 选择模型

1. 等待代理日志出现 `model catalog refreshed`。
2. 在 OpenCode 中打开 `/models`。
3. 选择 `cursor-proxy/composer-2.5` 或该 provider 下的其他模型。

连接地址和模型列表自动写入配置，无需执行 `/connect`。列表未更新时执行 `/reload`。

## 模型与配置

代理启动时获取模型列表，此后每小时刷新一次。刷新失败时保留上次结果，列表未变化时不重写配置。

支持 OpenCode V1 和 V2。代理启动时检查 `PATH` 中 `opencode --version` 的结果，按对应格式写入配置：`1.x` 使用 V1 格式，`2.x` 使用 V2 格式。识别失败时告警并使用 V2 格式。切换 OpenCode 安装后，重启代理以重新识别。

V1 的上下文限制为 `0` 时表示未知，可由观测到的容量补齐；已有的正数上下文限制和显式输出限制保持不变。

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

| 参数 | 用途 | 默认值 |
| --- | --- | --- |
| `--host` | 监听地址 | `127.0.0.1` |
| `--port` | 监听端口 | `32124` |
| `--password` | 设置访问密码，允许其他电脑访问时必填 | 未设置 |
| `--workspace` | 项目目录 | 当前目录 |
| `--cursor-agent` | Cursor CLI 路径 | `cursor-agent` |
| `--timeout-secs` | 回复超时秒数 | `300` |
| `--load-skills` | 加载 `AGENTS.md` 中的项目指令 | 关闭 |

代理使用 `--workspace` 指定的项目目录。切换项目时，停止代理，再用新目录启动。

## HTTP 接口

| 方法 | 路径 | 用途 |
| --- | --- | --- |
| GET | `/health` | 健康检查 |
| GET | `/v1/models` | 模型列表 |
| POST | `/v1/chat/completions` | 流式或非流式聊天 |

- 也支持省略 `/v1` 前缀。
- 仅支持文本输入和单条回复（`n=1`），暂不支持图片、音频、采样参数和 token 预算设置。
- 思考内容通过 `reasoning_content` 返回。统计优先使用上游分项用量；没有时，将当前 ACP 会话的已用上下文数换算为标准 `usage`，供 OpenCode 显示。
- 上下文回退使用最新已持久化计数，可能滞后；不虚构输出或缓存分项。窗口容量自动补入缺少该值的模型配置，保留已有设置。费用按已有模型单价估算，不代表 Cursor 实际账单；未配置单价时可能不显示费用。
- 模型实际可用性取决于 Cursor 账号权限和额度。

## 工具与模式

OpenCode 执行通过 MCP 桥转发的工具调用，结果回到原 ACP 对话后继续回复。Cursor 原生工具仍可能直接修改文件或执行命令，OpenCode 权限不覆盖这些操作。

工具结果续接无需额外参数。最终回复完成后，代理关闭该 ACP 对话；下一轮用户输入根据 OpenCode 提供的历史建立新对话。

代理根据 OpenCode 的模式提醒同步 Plan / Build；其他版本或自定义 agent 的兼容性需另行验证。

Plan 模式下，MCP 工具桥只允许 `read`、`glob`、`grep` 和 `question`，也不会为其他桥接工具授予 ACP 执行许可。写入计划目录、命令执行、子任务和自定义工具均不支持；Build 不受此限制。Cursor 原生运行时不受代理沙箱保护。

## 空回复恢复

上游正常结束但只有思考、没有正文，且未观测到工具活动时，代理在原对话中最多请求一次最终回答。不会重放原任务、自动切换模型或延长原请求超时；补答仍为空则报错。

架构、线程续接和恢复边界见 [DESIGN.md](DESIGN.md)。

## 许可

[GPL-3.0-only](LICENSE)。
