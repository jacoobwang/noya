# Noya

English | [简体中文](README-ZH.md)

Noya is a standalone coding-agent prototype focused on development tasks inside code repositories. It provides a lightweight agent runtime, tool calling, and event streaming that can be hosted by a CLI, HTTP service, or IDE integration.

## Architecture

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

`Agent` is the single deep-module interface a host needs to use: submit a user turn and consume the resulting `AgentEvent` stream. The LLM and tools are adapters behind stable seams, allowing future hosts such as `agentd`, WebSocket services, or IDE integrations.

The source tree is organized by responsibility:

```text
src/
  cli/      CLI arguments, login/logout, and TUI startup
  agent/    turn loop, events, cancellation, optional approval, and prompt
  llm/      OpenAI-compatible client, protocol DTOs, and SSE handling
  model/    model catalog, runtime configuration, and local credentials
  tools/    tool registry, filesystem tools, and command execution
  tui/      terminal host, state, input events, and rendering
```

## Usage

Sign in to a model before the first run. API keys are entered through a hidden prompt and are not displayed in the terminal:

```bash
cargo run -- login deepseek
# DeepSeek API key:

cargo run -- --workspace /path/to/repo
```

`login` makes the selected model active, so it does not need to be specified on subsequent runs. `--workspace` is optional and defaults to the current directory:

```bash
cargo run
```

Noya currently supports `openai`, `deepseek`, `qwen`, and `kimi`:

```bash
cargo run -- login openai
cargo run -- login deepseek
cargo run -- login qwen
cargo run -- login kimi
cargo run -- logout deepseek
cargo run -- logout          # Remove credentials for the active model
cargo run -- models          # Show supported models and login status
```

Example `models` output:

```text
MODEL     MODEL ID            STATUS
openai    gpt-4o              not logged in
deepseek  deepseek-v4-flash   not logged in
qwen      qwen3-coder-plus    active
kimi      kimi-k3             not logged in
```

Default model configuration:

| Model | Default endpoint | Default Model ID | API key environment variable |
| --- | --- | --- | --- |
| `openai` | `https://api.openai.com/v1` | `gpt-4o` | `OPENAI_API_KEY` |
| `deepseek` | `https://api.deepseek.com` | `deepseek-v4-flash` | `DEEPSEEK_API_KEY` |
| `qwen` | `https://dashscope.aliyuncs.com/compatible-mode/v1` | `qwen3-coder-plus` | `DASHSCOPE_API_KEY` |
| `kimi` | `https://api.moonshot.cn/v1` | `kimi-k3` | `MOONSHOT_API_KEY` |

Credentials are stored in `noya/credentials.json` under the current user's system configuration directory. The exact path is printed after a successful login. On Unix, the directory uses mode `0700` and the file uses mode `0600`. The environment variables above can also provide temporary credentials.

Noya starts in an inline TUI. Sent user messages are right-aligned and Agent output is left-aligned. Agent responses are streamed, rendered as Markdown, and written to native terminal scrollback while generation is still in progress. Supported Markdown includes headings, emphasis, inline code, code blocks, lists, blockquotes, and links. `/status` displays the active model and concrete Model ID.

To use another OpenAI-compatible endpoint, override the configuration explicitly:

```bash
cargo run -- --model qwen \
  --workspace /path/to/repo \
  --base-url https://dashscope.aliyuncs.com/compatible-mode/v1 \
  --model-id qwen3-coder-plus \
  --api-key ...
```

Command-line overrides take precedence over model defaults and saved credentials.

`qwen` defaults to the Alibaba Cloud China-compatible endpoint, while `kimi` defaults to Moonshot's China API. Use `--base-url` for other regions.

`--max-tool-loops` allows 50 tool-call rounds by default. After the limit is reached, one final model response is still allowed. If that response requests another tool, the current turn stops without executing the extra tool.

TUI commands:

```text
/help       Show help
/clear      Clear the current display history (native terminal scrollback remains)
/reset      Reset the session context
/status     Show workspace, model, and runtime state
/cancel     Cancel the active turn
/quit       Exit
```

`Ctrl+C` cancels an active turn and exits when idle. `Ctrl+D` always exits. All five built-in tools execute without confirmation. `run_command` executes non-interactive shell commands inside the workspace.

## Current Scope

Included: runtime turn loop, tool-loop guard, workspace-first prompt, LLM model adapter, and event streaming.

Not included: business domain models, domain plugins, business persistence, multi-tenant runtime management, or domain-specific checkpoint types.

Suggested next steps:

1. Add a sandbox adapter for `run_command` and optional approval policies for future high-risk tools.
2. Extract session, history, and compaction behind a standalone `SessionStore` seam.
3. Add a diff-aware patch tool to avoid replacing entire files.
4. Add an HTTP/SSE host while keeping the TUI as a transport adapter.
5. Add pause, resume, and replay while keeping the core runtime domain-independent.
