# Noya

[English](README.md) | 简体中文

Noya 是一个独立的 coding agent 原型，专注于处理代码仓库内的开发任务。它提供轻量的 agent runtime、工具调用和事件流能力，方便接入 CLI、HTTP 服务或 IDE。

## 当前内核

```text
CLI / future HTTP host
          |
       Agent  ← turn/tool loop + events
      /  |  \
Session Prompt ToolRegistry
   |      |       |
 JSONL workspace  read/list/search/patch/write/git/run
          |
       LlmClient (OpenAI-compatible)
```

`Agent` 是 host 提交 turn、消费 `AgentEvent` 的 runtime 接口；`Session` 是对话和模型上下文的持久事实来源。LLM 和工具继续作为稳定 seam 后面的 adapter。

核心目录按职责组织：

```text
src/
  cli/      CLI 参数、login/logout 和 TUI 启动入口
  agent/    turn loop、事件、取消、审批和 prompt
  llm/      OpenAI-compatible client、协议 DTO 和 SSE
  model/    model catalog、运行配置和本地凭证存储
  session/  append-only JSONL、重放 projection、恢复和压缩
  tools/    tool registry、文件/patch/Git 工具和命令工具
  tui/      terminal host、状态、输入事件和渲染
```

## 运行

首次使用先登录 model。API key 使用隐藏输入，不会显示在终端：

```bash
cargo run -- login deepseek
# DeepSeek API key:

cargo run -- --workspace /path/to/repo
```

`login` 会把该 model 设为当前活动 model；之后启动时不需要再次指定。`--workspace` 也可以省略，默认使用当前目录：

```bash
cargo run
```

目前支持 `openai`、`deepseek`、`qwen` 和 `kimi`：

```bash
cargo run -- login openai
cargo run -- login deepseek
cargo run -- login qwen
cargo run -- login kimi
cargo run -- logout deepseek
cargo run -- logout          # 删除当前活动 model 的凭证
cargo run -- models          # 查看支持的 model 和登录状态
```

`models` 输出示例：

```text
MODEL     MODEL ID            STATUS
openai    gpt-4o              not logged in
deepseek  deepseek-v4-flash   not logged in
qwen      qwen3-coder-plus    active
kimi      kimi-k3             not logged in
```

各 model 的默认配置：

| Model | 默认 endpoint | 默认 Model ID | API key 环境变量 |
| --- | --- | --- | --- |
| `openai` | `https://api.openai.com/v1` | `gpt-4o` | `OPENAI_API_KEY` |
| `deepseek` | `https://api.deepseek.com` | `deepseek-v4-flash` | `DEEPSEEK_API_KEY` |
| `qwen` | `https://dashscope.aliyuncs.com/compatible-mode/v1` | `qwen3-coder-plus` | `DASHSCOPE_API_KEY` |
| `kimi` | `https://api.moonshot.cn/v1` | `kimi-k3` | `MOONSHOT_API_KEY` |

凭证保存在系统用户配置目录的 `noya/credentials.json` 中，实际路径会在登录成功后输出；Unix 下目录权限为 `0700`，文件权限为 `0600`。也可以用上表中的环境变量临时提供凭证。

启动后进入 inline TUI。已经发送的用户消息右对齐，Agent 输出左对齐；Agent 回复会边生成、边渲染 Markdown、边写入终端原生 scrollback，不需要等待回答结束才能查看完整输出。支持标题、强调、行内代码、代码块、列表、引用和链接；`/status` 会显示当前 model 和实际 Model ID。

裸 `noya` 每次创建一个可持久恢复的本地 session。`noya resume` 恢复当前 workspace 最近的 session，也可以使用 ID 前缀恢复指定 session：

```bash
cargo run
cargo run -- resume
cargo run -- resume 019fbd63
cargo run -- sessions
cargo run -- sessions --all --json
cargo run -- session show 019fbd63
cargo run -- session export 019fbd63 --format markdown
cargo run -- session export 019fbd63 --format jsonl
cargo run -- session fork 019fbd63
cargo run -- session archive 019fbd63
```

设置 `NOYA_DATA_DIR` 时 session 保存在该目录，否则保存在操作系统本地数据目录的 `noya/` 下。每个 session 包含 append-only `events.jsonl`、派生的 `meta.json`、临时流式 checkpoint 和 advisory lock。Session 日志可能包含源码、prompt、模型 reasoning、工具参数和命令输出，应当视为敏感本地数据；API key 不会写入 session 文件。

需要接入其他 OpenAI-compatible endpoint 时，可以显式覆盖配置：

```bash
cargo run -- --model qwen \
  --workspace /path/to/repo \
  --base-url https://dashscope.aliyuncs.com/compatible-mode/v1 \
  --model-id qwen3-coder-plus \
  --api-key ...
```

命令行覆盖项优先于 model 默认值和已保存凭证。

`qwen` 默认使用阿里云中国区兼容地址，`kimi` 默认使用中国开放平台地址；其他区域可通过 `--base-url` 覆盖。

`--max-tool-loops` 默认允许 50 轮工具调用。达到上限后，Noya 会从最后一次 completion 中移除全部 tool definitions，并要求模型基于已经收集的结果回答；任何额外 tool call 都不会执行。

每个 tool call 默认最多执行 120 秒，进入模型上下文的序列化 tool result 默认限制为 32 KiB。可以使用 `--tool-timeout-seconds` 和 `--max-tool-output-bytes` 覆盖。

当上下文达到已知 model context window 的 75% 时，Noya 会自动压缩，并始终保留最近至少 4 个 completed turn。设置 `NOYA_AUTO_COMPACT=false` 可以关闭自动压缩，手动 `/compact` 仍然可用。

常用命令：

```text
/help       显示帮助
/new        创建并切换到新 session
/sessions   列出当前 workspace 的 session
/resume ID  使用 ID 前缀切换 session
/rename T   重命名当前 session
/retry      重试最近失败、取消或中断的输入
/compact    压缩旧上下文，同时保留完整 transcript
/clear      清空当前会话的显示记录（终端原生 scrollback 保留）
/reset      建立新的持久 context epoch，不删除历史
/status     显示 session、workspace、model、context 和运行状态
/cancel     取消当前 turn
/quit       退出
```

`Ctrl+C` 在 Agent 运行时取消当前 turn，空闲时退出；`Ctrl+D` 始终退出。当前 8 个内置 tool 均直接执行，不要求用户确认：

```text
read_file    读取完整 UTF-8 文件或 offset/limit 行范围
list_dir     列出 workspace 目录
search_text  使用 ripgrep 递归搜索
apply_patch  一次性执行经过验证、精确且无歧义的文本替换
write_file   创建或替换 UTF-8 文件
git_status   查看简洁的分支和工作区状态
git_diff     查看 staged/unstaged diff，可限制单个路径
run_command  在 workspace 中执行非交互 shell 命令
```

## 当前边界

包含：runtime turn loop、tool loop guard、workspace-first prompt、LLM model adapter、事件流、本地持久 session、崩溃恢复、resume/export/archive/fork、reset、retry 和 context compaction。

暂不包含：云同步、多用户共享 session、单 session 多 writer、tool 外部副作用的 exactly-once 恢复、transport event replay 和自动 secret redaction。

下一阶段建议按此顺序演进：

1. 为 `run_command` 增加 sandbox adapter，并为未来的高风险 tool 提供可选 policy。
2. 增加 HTTP/SSE host，并严格区分 durable session replay 与 transport replay。
3. 增加 opt-in secret redaction 和远程备份 adapter，同时保留本地 JSONL 事实来源。
