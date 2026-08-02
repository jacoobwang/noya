# Noya TUI Specification

## 1. 目标

为 Noya 提供一个可持续交互的终端界面，让用户能够：

- 提交 coding agent 任务；
- 观察 Agent 回复和工具执行进度；
- 在同一个终端会话中继续提交任务；
- 查看历史输出并处理错误；
- 为后续的工具审批、取消和会话管理保留清晰接口。

第一版采用 inline viewport：输入区固定在终端底部，对话内容在生成过程中持续写入终端原生 scrollback。

## 2. 非目标

第一版暂不实现：

- 全屏可分页的聊天历史面板；
- 按编程语言进行代码语法高亮；
- 多会话切换和会话持久化；
- 多个 Agent 任务并发执行。

## 3. 用户体验

### 3.1 布局

```text
历史消息：写入终端 scrollback
----------------------------------------
状态栏：Ready / Thinking / Running tool
输入框：> 输入任务
```

底部 viewport 至少包含两行：状态栏和输入栏。状态栏显示 Agent 状态及临时提示，输入栏显示 prompt、输入内容和光标。

### 3.2 消息类型

| 类型 | 用途 |
| --- | --- |
| User | 用户提交的任务 |
| Agent | Agent 文本回复 |
| Tool | 工具开始、进行中和完成状态 |
| System | 帮助、重置、生命周期提示 |
| Error | 可恢复或不可恢复的错误 |

消息必须带稳定 ID，以便更新正在执行的工具消息或生成中的 Agent 消息。

### 3.3 交互

| 按键 | 行为 |
| --- | --- |
| Enter | 提交当前输入 |
| Ctrl+C | Agent 运行时取消当前 turn，空闲时退出 |
| Ctrl+D | 退出 TUI |
| Backspace / Delete | 编辑输入 |
| Left / Right | 移动光标 |
| Home / End | 移动到输入首尾 |
| Tab | 补全 slash command |

第一版命令：

```text
/help       显示帮助
/clear      清空当前会话的显示记录；终端原生 scrollback 保留
/reset      重建 Agent 会话上下文
/status     显示 workspace、model 和当前状态
/cancel     取消当前 turn
/quit       退出
```

空输入不提交。Agent 执行期间不允许提交第二个普通任务，避免同一个 Agent 实例的消息上下文发生并发修改。

## 4. 模块结构

```text
src/
  main.rs
  lib.rs
  cli/
    mod.rs       CLI 参数、model 登录/登出/列表和 host 启动
  agent/
    mod.rs       turn loop 和 Agent 外部接口
    event.rs     AgentEvent 协议
    control.rs   取消和工具审批
    prompt.rs    workspace-first system prompt
  llm/
    mod.rs       OpenAI-compatible client
    protocol.rs  请求、响应和 tool-call DTO
    stream.rs    SSE 解码和增量组装
  model/
    mod.rs       model catalog、运行配置和凭证存储
  tools/
    mod.rs       Tool interface 和 registry
    filesystem.rs
    patch.rs
    git.rs
    command.rs
  tui/
    mod.rs       terminal 初始化、生命周期和主循环
    app.rs       UI 状态、状态转换和命令结果
    event.rs     键盘事件、Tick 和事件桥接
    markdown.rs  Markdown 解析、终端样式和宽度换行
    ui.rs        inline viewport 和消息渲染
```

### 4.1 `TuiApp` 模块

`TuiApp` 是 TUI 的深模块。它对外只需要处理两类输入：

```rust
pub fn handle_key(&mut self, event: KeyEvent) -> TuiAction;
pub fn handle_agent_event(&mut self, event: AgentEvent);
```

它负责隐藏输入编辑、消息历史、Agent 状态、正在生成的消息、工具调用状态、slash command 解析、临时状态提示和提交互斥规则。

渲染器只读取 `TuiApp` 状态，不直接修改状态。

### 4.2 Terminal Adapter

`tui::mod` 负责 terminal adapter：

- 启用和恢复 raw mode；
- 初始化 `CrosstermBackend`；
- 创建 inline viewport；
- 捕获 panic 并恢复 terminal；
- 运行 `tokio::select!` 主循环；
- 将已完成消息写入 scrollback；
- 渲染底部 viewport。

Agent 核心模块不能依赖 ratatui 或 crossterm。

## 5. 事件模型

### 5.1 Agent 事件

第一版需要支持文本增量事件，并建议为工具事件增加稳定 ID：

```rust
pub enum AgentEvent {
    TurnStarted,
    TextDelta {
        chunk: String,
        is_final: bool,
    },
    ToolStarted {
        call_id: String,
        name: String,
        arguments: serde_json::Value,
    },
    ToolFinished {
        call_id: String,
        name: String,
        result: serde_json::Value,
        success: bool,
    },
    ApprovalRequired {
        request_id: String,
        call_id: String,
        tool_name: String,
        arguments: serde_json::Value,
    },
    TurnCompleted,
    Error(String),
}
```

`TextDelta` 用于实时更新当前 Agent 消息。`is_final=true` 表示本次消息结束，TUI 应将该消息标记为完成并写入 terminal scrollback。对于不支持流式的模型服务，`LlmClient` 可以把完整响应包装成一个 `TextDelta { is_final: true }` 事件。

### 5.2 TUI 事件

```rust
pub enum TuiEvent {
    Key(crossterm::event::KeyEvent),
    Tick,
    Resize(u16, u16),
    Agent(AgentEvent),
}
```

键盘事件由独立的 `EventHandler` 读取，并通过 Tokio channel 进入主循环。Agent 事件也通过 channel 进入主循环，主循环是唯一修改 `TuiApp` 的位置。

### 5.3 用户动作

```rust
pub enum TuiAction {
    None,
    Submit(String),
    Clear,
    Reset,
    Cancel,
    Approval(ApprovalDecision),
    Quit,
}
```

`TuiApp` 不直接启动 Agent task，也不直接执行工具。它只产生动作，由主循环或 host 负责执行。

## 6. 状态模型

```rust
pub enum AgentState {
    Idle,
    Thinking,
    Generating,
    RunningTool,
    WaitingApproval,
    Error,
}
```

典型状态转换：

```text
Idle
  └─ Submit ───────> Thinking
                       ├─ ToolStarted ──> RunningTool
                       │                   └─ ToolFinished ──> Thinking
                       ├─ TextDelta ─────> Generating
                       │                   └─ is_final ──> Idle
                       └─ Error ─────────> Error
Error
  └─ New Submit ───> Thinking
```

`TurnCompleted` 是回到 `Idle` 的最终信号。即使某个工具失败，TUI 也必须保持可用，错误只影响当前 turn。

## 7. Agent 通信

TUI 与 Agent 之间使用 Tokio channel。文本和工具事件都必须在执行过程中及时发送：

```text
TUI input
   │
   ├── Submit(String) ──> Agent task
   │
   └<── AgentEvent       Agent task
```

Agent task 持有唯一的 `Agent` 可变引用。TUI 不直接跨线程共享或复制 Agent。

初步接口可以保持现有 callback 形式，在 TUI host 中转发事件：

```rust
agent
    .turn(input, |event| {
        let _ = event_tx.send(event);
    })
    .await?;
```

如果后续需要取消、背压或可靠错误传递，再将 Agent 的事件输出正式抽象成 sender/stream adapter。

## 8. 流式输出

### 8.1 LLM Adapter

`LlmClient` 需要提供流式 completion 接口，使用 OpenAI-compatible SSE 响应：

```rust
pub async fn complete_stream(
    &self,
    messages: Vec<ChatMessage>,
    tools: Vec<ToolDefinition>,
    temperature: f32,
    emit: impl FnMut(LlmEvent),
) -> Result<ChatStreamResponse>;
```

流式接口需要处理：

- SSE data chunk 解码；
- `delta.content` 增量文本；
- `delta.reasoning_content` 增量组装，并在工具调用后的 assistant 消息中原样回传；
- tool call 参数的增量拼接；
- `[DONE]` 结束标记；
- 模型服务错误和连接中断；
- 不支持 stream 的模型服务 fallback。

Model adapter 负责补齐厂商兼容差异。例如 Kimi K3 的思考状态需要跨工具调用保留，并且请求不发送通用 `temperature` 参数；这些差异不应泄漏到 TUI 状态层。

### 8.2 Agent Adapter

Agent 在收到每个文本 chunk 后立即发送：

```rust
AgentEvent::TextDelta {
    chunk,
    is_final: false,
}
```

`max_tool_loops` 统计实际执行的工具轮次，而不是 LLM 请求数。达到上限后 Agent 发起一次强制最终 completion：请求不再携带 tool definitions，并附加基于已有结果完成回答的 system instruction。Provider 即使违规返回 tool call，也不得执行该调用或把整个 turn 转成 recoverable error。

每次 tool call 受统一 timeout 限制；超时转换为结构化失败结果并写回模型上下文。序列化 tool result 超过上限时替换为包含 `truncated`、`original_bytes` 和 `preview` 的有界结果，避免单次工具输出耗尽上下文。

流结束时发送最后一个 `is_final: true` 事件，随后发送 `TurnCompleted`。TUI 不应等待 `TurnCompleted` 才显示文本。

当响应包含 tool call 时，Agent 需要先完整收集并解析 tool call，再开始执行工具。工具执行期间继续发送 `ToolStarted`、`ToolFinished` 事件；工具结果进入下一次 LLM completion。

### 8.3 TUI 增量渲染

`TuiApp` 维护 `streaming_message_id`：

1. 收到第一个 `TextDelta` 时创建空的 Agent 消息；
2. 后续 chunk 追加到同一消息；
3. `is_final=true` 时关闭 streaming 状态；
4. 消息只写入一次 terminal scrollback；
5. Agent 生成期间状态栏显示 `Generating` 和 spinner。

增量渲染必须避免每个 chunk 都重复打印整条消息。可以在 TUI 状态中更新消息，并按 Tick 或消息完成时刷新 terminal 输出。

### 8.4 中断和部分消息

如果连接中断或用户取消当前 turn：

- 保留已经收到的部分文本；
- 将消息标记为 interrupted 或 error；
- 在消息末尾显示中断原因；
- 恢复输入状态，允许用户继续提交任务。

## 9. 渲染规则

### 9.1 历史消息

每条消息已经形成的显示行会在生成过程中持续写入 terminal scrollback，底部 viewport 只保留当前尚未完成的最后一行。TUI runtime 按消息维护已提交行数和初始渲染宽度，避免每次重绘重复输出，也避免 resize 改变换行边界后造成漏行或重复行。消息完成时只补写剩余内容和消息间隔。

使用 inline viewport 的 `insert_before` 写入 scrollback 时，宽字符占用的 continuation cell 不得作为真实空格输出；中文、日文、韩文和双宽 emoji 应保持原始连续文本。

### 9.2 Agent 消息

Agent 回复在生成过程中保存在 App 状态中，并将已经形成的显示行增量写入 scrollback。生成中的最后一行直接显示在状态栏上方，不使用限制正文高度的边框；状态栏继续显示 generating spinner。

对话按说话方区分方向：已经发送的用户消息右对齐，Agent 的角色名、Markdown 正文和流式尾行左对齐。输入编辑框、工具、系统和错误消息保持左对齐。

Agent 消息使用同一套 Markdown renderer 渲染流式预览和已完成消息，支持标题、强调、删除线、行内代码、代码块、列表、引用、链接、分隔线和表格。流式消息允许 Markdown 结构暂时未闭合，每次刷新基于已收到的完整文本重新解析。代码块保留空白和缩进，但第一版不按编程语言做语法高亮。

### 9.3 工具消息

工具开始时显示工具名称和参数；完成时更新为成功或失败状态。详细结果默认截断或折叠，避免大段 stdout、文件内容直接淹没对话历史。

### 9.4 状态栏

状态栏至少支持：

```text
Ready
Thinking
Running tool: read_file
Error: LLM request failed
```

spinner 由 Tick 驱动，不需要 Agent 产生额外动画事件。

## 10. 错误和生命周期

### Terminal 生命周期

- 初始化失败时返回错误，不进入 Agent loop；
- 正常退出时恢复 raw mode；
- panic 时通过 panic hook 恢复 terminal；
- TUI 运行失败后再次尝试恢复 terminal，再向上层返回错误。

### Agent 错误

- LLM 请求失败显示 `Error` 消息；
- 工具失败显示对应工具的失败状态；
- turn 失败后允许用户继续输入；
- 不把单次 Agent 错误转换成整个进程退出。

## 11. 工具审批

当前注册的 `read_file`、`write_file`、`list_dir`、`search_text`、`apply_patch`、`git_status`、`git_diff` 和 `run_command` 均直接执行，不要求用户审批。

`Tool::requires_approval()` 及以下事件保留为扩展能力，但没有内置 tool 启用该能力。未来新增的高风险 tool 可以显式返回 `true`，执行前发出：

```rust
AgentEvent::ApprovalRequired {
    request_id: String,
    tool_name: String,
    arguments: serde_json::Value,
}
```

可选审批流程：

```text
ApprovalRequired
      ↓
TuiMode::Confirming
      ↓
用户选择 approve / reject / modify
      ↓
TuiAction::Approve / Reject / Modify
      ↓
Agent command channel
```

如果未来的 tool 启用审批，拒绝时不执行工具，并将结构化拒绝结果写回 Agent 上下文；修改参数时使用用户提供的新参数执行工具。

## 12. 实现阶段

### Phase 1：Terminal 和流式假数据

- 添加 ratatui、crossterm 依赖；
- 实现 raw mode 和 terminal restore；
- 实现 inline viewport；
- 实现输入框、状态栏和增量消息渲染；
- 用假的 `TextDelta` 事件验证消息追加、完成和中断状态；
- 为输入编辑和命令解析添加单元测试。

### Phase 2：接入流式 Agent

- 添加 Agent event channel；
- 运行 Agent task；
- 实现 LLM SSE response reader；
- 渲染 `TextDelta`、`ToolStarted`、`ToolFinished`、`TurnCompleted`；
- 拼接增量 tool call 参数；
- 支持非流式模型服务 fallback；
- 实现单任务互斥；
- 验证连接中断和错误后可继续交互。

### Phase 3：会话操作

- 实现 `/help`、`/clear`、`/reset`、`/status`、`/quit`；
- 添加初始 workspace/model 状态；
- 添加历史消息数量上限；
- 完善 UTF-8 输入和终端 resize 处理。

### Phase 4：取消和可选审批基础设施

- 增加稳定工具调用 ID；
- 增加审批事件和 command channel；
- Ctrl+C 改为取消当前 turn；
- 验证默认内置 tool 不触发审批，并保留可选审批状态测试。

## 13. 验收标准

第一版完成时必须满足：

1. 能从 TUI 启动并正常恢复 terminal；
2. 用户可以提交任务并收到 Agent 回复；
3. 工具开始和完成状态可见；
4. Agent 执行期间重复提交会被阻止；
5. Agent 或工具失败后 TUI 仍可继续使用；
6. 历史消息不会在每次重绘时重复打印；
7. `/help`、`/clear`、`/reset`、`/status`、`/quit` 可用；
8. 输入编辑对中文和其他 UTF-8 字符安全；
9. 单元测试覆盖 SSE 解码、增量文本拼接、事件 reducer、命令解析和关键状态转换。
10. 所有默认内置 tool 均可直接执行，不产生审批事件；
11. 用户可以取消当前 turn，并保留已经接收的部分输出。
12. tool loop 达到上限后强制生成最终回答，不执行超额工具；
13. tool timeout 和输出上限会产生模型可见的结构化结果；
14. `apply_patch` 在上下文缺失或有歧义时不产生部分写入。
