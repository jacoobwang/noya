# Noya

[English](README.md) | 简体中文

Noya 是一个独立的 coding agent 原型，专注于处理代码仓库内的开发任务。它提供轻量的 agent runtime、工具调用和事件流能力，方便接入 CLI、HTTP 服务或 IDE。

## 当前内核

```text
CLI / future HTTP host
          |
       Agent  ← session + turn/tool loop + events
       /   \
  Prompt   ToolRegistry
    |       |
 workspace  read/write/list/search/run
          |
       LlmClient (OpenAI-compatible)
```

`Agent` 是唯一需要被 host 使用的深模块接口：输入一个 user turn，持续发出 `AgentEvent`。LLM 和工具都在 seam 上作为 adapter，后面可以接 `agentd`、WebSocket 或 IDE host。

核心目录按职责组织：

```text
src/
  cli/      CLI 参数、login/logout 和 TUI 启动入口
  agent/    turn loop、事件、取消、审批和 prompt
  llm/      OpenAI-compatible client、协议 DTO 和 SSE
  model/    model catalog、运行配置和本地凭证存储
  tools/    tool registry、文件工具和命令工具
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

`--max-tool-loops` 默认允许 50 轮工具调用。达到上限后仍允许一次最终模型响应；如果该响应继续请求工具，则终止当前 turn，且不会执行超额工具。

常用命令：

```text
/help       显示帮助
/clear      清空当前会话的显示记录（终端原生 scrollback 保留）
/reset      重置会话上下文
/status     显示 workspace、model 和运行状态
/cancel     取消当前 turn
/quit       退出
```

`Ctrl+C` 在 Agent 运行时取消当前 turn，空闲时退出；`Ctrl+D` 始终退出。当前 5 个内置 tool 均直接执行，不要求用户确认。`run_command` 会在 workspace 中执行非交互 shell 命令。

## 当前边界

包含：runtime turn loop、tool loop guard、workspace-first prompt、LLM model adapter 和事件流。

暂不包含：业务领域模型、领域插件、业务 persistence、多业务实例管理，以及面向特定业务的 checkpoint 类型。

下一阶段建议按此顺序演进：

1. 为 `run_command` 增加 sandbox adapter，并为未来的高风险 tool 提供可选 approval policy。
2. 把 session/history/compaction 抽成独立 `SessionStore` seam。
3. 增加 diff-aware patch 工具，避免模型整文件覆盖。
4. 增加 HTTP/SSE host；TUI 只保留 transport adapter。
5. 增加 pause/resume/replay 能力，并保持核心 runtime 与具体业务领域解耦。
