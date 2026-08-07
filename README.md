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
 JSONL workspace  read/list/search/navigation/patch/write/git/run
          |
       LlmClient (multi-protocol provider adapter)
```

`Agent` is the runtime interface a host uses to submit turns and consume `AgentEvent` streams. `Session` is the durable source of conversation and model context; the LLM and tools remain adapters behind stable seams.

The source tree is organized by responsibility:

```text
src/
  cli/      CLI arguments, login/logout, and TUI startup
  agent/    turn loop, events, cancellation, optional approval, and prompt
  llm/      provider adapters, protocol DTOs, and SSE handling
  model/    model catalog, runtime configuration, and local credentials
  session/  append-only JSONL, replay projections, recovery, and compaction
  tools/    tool registry, filesystem/patch/Git tools, and command execution
  tui/      terminal host, state, input events, and rendering
```

## Install a prebuilt release

Install the latest binary without Rust or a source checkout:

```bash
curl -fsSL https://raw.githubusercontent.com/jacoobwang/noya/main/scripts/install.sh | sh
```

Noya currently publishes macOS binaries for Apple Silicon and Intel. The installer detects the CPU architecture, verifies the release against `SHA256SUMS`, installs `noya` to `~/.local/bin` by default, and adds that directory to the detected Bash or Zsh configuration when it is not already on `PATH`. Restart the shell after installation (or run the command printed by the installer). If `rg` is missing and Homebrew is available, it also runs `brew install ripgrep`. It does not install Homebrew automatically.

Optional overrides:

```bash
NOYA_VERSION=v0.2.0 NOYA_INSTALL_DIR=/usr/local/bin sh scripts/install.sh
NOYA_SKIP_RIPGREP=1 sh scripts/install.sh
```

An installed Noya binary can update or remove itself:

```bash
noya --version             # show the current version
noya upgrade                 # install the latest release
noya upgrade --version v0.3.0
noya uninstall               # confirm before removing the binary
noya uninstall --yes         # remove without confirmation
```

Uninstall removes only the executable. Noya configuration and sessions under `~/.noya` are preserved.

Published releases must provide `noya-<rust-target>.tar.gz` archives containing a top-level `noya` executable and a matching `SHA256SUMS` file.

## Usage

After installing the prebuilt release, run the `noya` executable directly. Sign in to a model before the first run; API keys are entered through a hidden prompt and are not displayed in the terminal:

```bash
noya login deepseek
# DeepSeek API key:

cd /path/to/repo
noya

# Or start Noya for a specific repository from any directory:
noya --workspace /path/to/repo
```

`noya login <model>` saves the provider protocol, authentication mode, `base_url`, and API key. Use `--protocol` and `--auth-mode` to select non-default protocol or authentication behavior.

`login` makes the selected model active, so it does not need to be specified on subsequent runs. `--workspace` is optional and defaults to the current directory:

```bash
noya
```

When running from a source checkout, use `cargo run --` before Noya's arguments. For example, `noya login deepseek` becomes `cargo run -- login deepseek`, while a bare `noya` becomes `cargo run`.

Noya currently supports `openai`, `deepseek`, `qwen`, `kimi`, and `claude`. Claude defaults to the native Anthropic Messages protocol; the other providers default to the OpenAI-compatible protocol:

```bash
noya login openai
noya login deepseek
noya login qwen
noya login kimi
noya logout deepseek
noya logout          # Remove credentials for the active model
noya models          # Show supported models and login status
```

Example `models` output:

```text
MODEL     MODEL ID            STATUS
openai    gpt-4o              not logged in
deepseek  deepseek-v4-flash   not logged in
qwen      qwen3-coder-plus    active
kimi      kimi-k3             not logged in
claude    claude-sonnet-4.5           not logged in
```

Default model configuration:

| Model | Default endpoint | Default Model ID | Protocol | Authentication | API key environment variable |
| --- | --- | --- | --- | --- | --- |
| `openai` | `https://api.openai.com/v1` | `gpt-4o` | `openai-compatible` | `bearer` | `OPENAI_API_KEY` |
| `deepseek` | `https://api.deepseek.com` | `deepseek-v4-flash` | `openai-compatible` | `bearer` | `DEEPSEEK_API_KEY` |
| `qwen` | `https://dashscope.aliyuncs.com/compatible-mode/v1` | `qwen3-coder-plus` | `openai-compatible` | `bearer` | `DASHSCOPE_API_KEY` |
| `claude` | `https://api.anthropic.com/v1` | `claude-sonnet-4.5` | `anthropic-messages` | `x-api-key` | `ANTHROPIC_API_KEY` |
| `kimi` | `https://api.moonshot.cn/v1` | `kimi-k3` | `openai-compatible` | `bearer` | `MOONSHOT_API_KEY` |

Credentials are stored in `~/.noya/credentials.json` under the current user's home directory. Set `NOYA_CONFIG_DIR` to use another credentials directory. The exact path is printed after a successful login. On Unix, the directory uses mode `0700` and the file uses mode `0600`. The environment variables above can also provide temporary credentials.

Each provider can configure its protocol, authentication mode, endpoint, and model ID. Protocols are `openai-compatible` and `anthropic-messages`; authentication modes are `bearer` and `x-api-key`.

For example, configure the local native Anthropic service with:

```bash
noya login claude --protocol anthropic-messages --auth-mode bearer
# Base URL: http://192.168.0.6:3000/v1
```

```json
{
  "active_model": "deepseek",
  "models": {
    "openai": {
      "api_key": "sk-...",
      "base_url": "https://api.openai.com/v1",
      "model_id": "gpt-4o",
      "protocol": "openai-compatible",
      "authentication": "bearer"
    },
    "deepseek": {
      "api_key": "sk-...",
      "base_url": "https://gateway.example/v1",
      "model_id": "deepseek-custom",
      "protocol": "openai-compatible",
      "authentication": "bearer"
    }
  }
}
```

Noya also keeps the model IDs discovered from each configured endpoint in `~/.noya/models.json` (or under `NOYA_CONFIG_DIR`). The catalog is keyed by provider and normalized `base_url`; API keys are never written to it. At startup, Noya reads the cached catalog immediately and refreshes configured providers asynchronously through `GET <base_url>/models`. If discovery fails, the previous catalog remains available and the next startup retries automatically.

The catalog uses this shape:

```json
{
  "catalogs": {
    "openai@https://gateway.example/v1": {
      "provider": "openai",
      "base_url": "https://gateway.example/v1",
      "fetched_at": "2026-08-04T08:00:00Z",
      "models": ["gpt-4o", "gpt-4.1"]
    }
  }
}
```

Command-line options take precedence over provider settings, which take precedence over built-in defaults. `noya login <model>` saves that provider's protocol, authentication mode, endpoint, and API key.

Noya starts in an inline TUI with a welcome header showing the installed version, active model and Model ID, and workspace directory. Type `/` to open the command menu, use ↑/↓ to select a command, Enter to apply it, Tab to complete it without running, and Esc to close the menu. Sent user messages are right-aligned and Agent output is left-aligned. Agent responses are streamed, rendered as Markdown, and written to native terminal scrollback while generation is still in progress. Supported Markdown includes headings, emphasis, inline code, code blocks, lists, blockquotes, and links. `/status` displays the active model and concrete Model ID.

Use `/model` to open an interactive picker containing all supported providers and discovered model IDs. Selecting an unconfigured provider starts an in-TUI setup that asks for protocol, `base_url`, authentication mode, and a hidden API key, then discovers and displays the endpoint's model IDs. Select with ↑/↓ and Enter, or cancel with Esc.

Each bare `noya` run creates a durable local session. `noya resume` continues the latest session for the current workspace; an ID prefix resumes a specific session:

```bash
noya
noya resume
noya resume 019fbd63
noya sessions
noya sessions --all --json
noya session show 019fbd63
noya session export 019fbd63 --format markdown
noya session export 019fbd63 --format jsonl
noya session fork 019fbd63
noya session tree 019fbd63
noya session branch-create 019fbd63 experiment
noya session branch-select 019fbd63 <branch-uuid> --summary "handoff notes"
noya session archive 019fbd63
```

Session data is stored below `NOYA_DATA_DIR` when set, otherwise in `~/.noya/`. Each session has an append-only `events.jsonl`, derived `meta.json`, a transient streaming checkpoint, and an advisory lock. Session logs may contain source code, prompts, model reasoning, tool arguments, and command output; protect them as sensitive local data. API keys are never written to session files.

To use another OpenAI-compatible endpoint, override the configuration explicitly:

```bash
noya --model qwen \
  --workspace /path/to/repo \
  --base-url https://dashscope.aliyuncs.com/compatible-mode/v1 \
  --model-id qwen3-coder-plus \
  --api-key ...
```

Command-line overrides take precedence over model defaults and saved credentials.

`qwen` defaults to the Alibaba Cloud China-compatible endpoint, while `kimi` defaults to Moonshot's China API. Use `--base-url` for other regions.

`--max-tool-loops` allows 50 tool-call rounds by default. At the limit, Noya removes all tool definitions from one final completion and instructs the model to answer from the results already collected. Extra tool calls are never executed.

Each tool call has a 120-second timeout by default, and serialized tool results are limited to 32 KiB before entering the model context. Override these limits with `--tool-timeout-seconds` and `--max-tool-output-bytes`.

Mutating tools require approval by default. `--tool-approval never|mutating|always` (or `NOYA_TOOL_APPROVAL`) controls the policy; `--blocked-tools` (or `NOYA_BLOCKED_TOOLS`, comma-separated) prevents selected tools from executing. The default `mutating` policy requires approval for `apply_patch`, `write_file`, and `run_command`.

`/status` reports cumulative tool calls, elapsed time, and provider usage when available. If the provider does not return usage, Noya shows a clearly marked character-based estimate. Set `NOYA_INPUT_COST_PER_MILLION_USD` and `NOYA_OUTPUT_COST_PER_MILLION_USD` to enable estimated cost reporting.

Noya automatically compacts context at 75% of a known model context window and always preserves at least the four latest completed turns. Set `NOYA_AUTO_COMPACT=false` to disable automatic compaction; `/compact` remains available.

TUI commands:

```text
/help       Show help
/new        Create and switch to a new session
/model      Choose a logged-in model, or switch with /model <name>
/sessions   List sessions for the current workspace
/tree       Show the current session tree and named branches
/branch N   Create branch N; use `/branch select ID [summary]` to switch
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

`Ctrl+C` cancels an active turn and exits when idle. `Ctrl+D` always exits. Read-only tools run directly; mutating tools follow the approval policy:

```text
read_file    Read a whole UTF-8 file or an offset/limit line range
list_dir     List a workspace directory
search_text  Search recursively with ripgrep
code_navigation  Find definitions, references, or workspace symbols through a local LSP
apply_patch  Apply a validated batch of exact, unambiguous text replacements
write_file   Create or replace a UTF-8 file
git_status   Show concise branch and working-tree status
git_diff     Show staged or unstaged changes, optionally for one path
run_command  Run a non-interactive shell command in the workspace
```

`code_navigation` uses a locally installed language server when available. Noya currently recognizes Rust, C/C++, Go, Python, TypeScript/JavaScript, Java, Lua, and Bash; Java uses Eclipse JDT Language Server (`jdtls`). Servers are started lazily per workspace and language; the current `--workspace` is always used as the LSP root. Set a user-level override such as `NOYA_LSP_RUST=/custom/bin/rust-analyzer` or `NOYA_LSP_JAVA=/custom/bin/jdtls` when a server is not on `PATH`. `workspace_symbols` falls back to plain-text search when its language server is unavailable; definitions and references return an explicit LSP error instead of pretending text matches are semantic results.

## Current Scope

Included: runtime turn loop, tool-loop guard, workspace-first prompt, LLM model adapter, event streaming, durable local sessions, crash recovery, resume/export/archive/fork, reset, retry, and context compaction.

Not included: cloud synchronization, multi-user session sharing, multiple writers for one session, exactly-once recovery of tool side effects, transport-event replay, or automatic secret redaction. Cost rates are opt-in and configured globally rather than maintained in a model catalog.

Suggested next steps:

1. Add a sandbox adapter for `run_command` and per-workspace policy files.
2. Add an HTTP/SSE host while keeping durable session replay separate from transport replay.
3. Add opt-in secret redaction and remote backup adapters without changing the local JSONL source of truth.
