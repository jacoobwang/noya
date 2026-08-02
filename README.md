# Noya

English | [简体中文](README-ZH.md)

Noya is a standalone coding-agent prototype focused on development tasks inside code repositories. It provides a lightweight agent runtime, tool calling, and event streaming that can be hosted by a CLI, HTTP service, or IDE integration.

## Architecture

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

`Agent` is the runtime interface a host uses to submit turns and consume `AgentEvent` streams. `Session` is the durable source of conversation and model context; the LLM and tools remain adapters behind stable seams.

The source tree is organized by responsibility:

```text
src/
  cli/      CLI arguments, login/logout, and TUI startup
  agent/    turn loop, events, cancellation, optional approval, and prompt
  llm/      OpenAI-compatible client, protocol DTOs, and SSE handling
  model/    model catalog, runtime configuration, and local credentials
  session/  append-only JSONL, replay projections, recovery, and compaction
  tools/    tool registry, filesystem/patch/Git tools, and command execution
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

Each bare `noya` run creates a durable local session. `noya resume` continues the latest session for the current workspace; an ID prefix resumes a specific session:

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

Session data is stored below `NOYA_DATA_DIR` when set, otherwise in the operating system's local data directory under `noya/`. Each session has an append-only `events.jsonl`, derived `meta.json`, a transient streaming checkpoint, and an advisory lock. Session logs may contain source code, prompts, model reasoning, tool arguments, and command output; protect them as sensitive local data. API keys are never written to session files.

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

`--max-tool-loops` allows 50 tool-call rounds by default. At the limit, Noya removes all tool definitions from one final completion and instructs the model to answer from the results already collected. Extra tool calls are never executed.

Each tool call has a 120-second timeout by default, and serialized tool results are limited to 32 KiB before entering the model context. Override these limits with `--tool-timeout-seconds` and `--max-tool-output-bytes`.

Noya automatically compacts context at 75% of a known model context window and always preserves at least the four latest completed turns. Set `NOYA_AUTO_COMPACT=false` to disable automatic compaction; `/compact` remains available.

TUI commands:

```text
/help       Show help
/new        Create and switch to a new session
/sessions   List sessions for the current workspace
/resume ID  Switch to a session matching an ID prefix
/rename T   Rename the active session
/retry      Retry the latest failed, cancelled, or interrupted input
/compact    Summarize older context while retaining the full transcript
/clear      Clear the current display history (native terminal scrollback remains)
/reset      Start a new durable context epoch without deleting history
/status     Show session, workspace, model, context, and runtime state
/cancel     Cancel the active turn
/quit       Exit
```

`Ctrl+C` cancels an active turn and exits when idle. `Ctrl+D` always exits. All eight built-in tools execute without confirmation:

```text
read_file    Read a whole UTF-8 file or an offset/limit line range
list_dir     List a workspace directory
search_text  Search recursively with ripgrep
apply_patch  Apply a validated batch of exact, unambiguous text replacements
write_file   Create or replace a UTF-8 file
git_status   Show concise branch and working-tree status
git_diff     Show staged or unstaged changes, optionally for one path
run_command  Run a non-interactive shell command in the workspace
```

## Current Scope

Included: runtime turn loop, tool-loop guard, workspace-first prompt, LLM model adapter, event streaming, durable local sessions, crash recovery, resume/export/archive/fork, reset, retry, and context compaction.

Not included: cloud synchronization, multi-user session sharing, multiple writers for one session, exactly-once recovery of tool side effects, transport-event replay, or automatic secret redaction.

Suggested next steps:

1. Add a sandbox adapter for `run_command` and optional policies for future high-risk tools.
2. Add an HTTP/SSE host while keeping durable session replay separate from transport replay.
3. Add opt-in secret redaction and remote backup adapters without changing the local JSONL source of truth.
