# Noya Session Specification

## 1. 目标

为 Noya 提供完整、可恢复、可审计的本地 session 能力，让用户能够：

- 在不同进程之间恢复对话和 Agent 上下文；
- 查看、继续、重命名、重置、压缩、归档和导出 session；
- 在模型、工具、网络或进程异常后保留已经完成的工作记录；
- 保证恢复后的 assistant tool call 与 tool result 仍满足模型协议；
- 保留完整原始日志，同时为模型生成受控、可压缩的上下文投影；
- 在不引入 SQLite 或其他数据库的前提下完成全部本地持久化。

Session 日志采用本地 append-only JSONL。JSONL 是事实来源，其他文件都是可重建的索引、缓存或运行中 checkpoint。

## 2. 非目标

第一阶段不实现：

- 云端 session 同步；
- 多用户或多租户 session；
- 多进程协同写入同一个 session；
- 基于数据库的查询、全文检索或统计；
- 对运行中 tool call 的 exactly-once 崩溃恢复；
- 在同一个 session 内移动上下文 head 的复杂分支图；
- 持久化 API key、Authorization header 或其他 credential；
- 为 HTTP/SSE/WebSocket 客户端保存独立 transport event history。

未来增加远程 host 时，可以在 session 语义日志之外增加 transport replay adapter，但两种重放不能混为一体：

- session replay 恢复 transcript 和模型上下文；
- transport replay 恢复客户端事件游标和增量推送位置。

## 3. 核心设计决策

### 3.1 使用本地文件而不是数据库

本地单用户 coding agent 的主要操作是：

- 顺序追加事件；
- 按 session ID 打开；
- 按 workspace 和更新时间列出 session；
- 从头重放单个 session。

这些访问模式不需要关系查询和事务数据库。使用一个 session 目录、一份 JSONL 日志和一个派生 `meta.json`，可以获得更简单的备份、迁移、检查和故障恢复能力。

### 3.2 日志是唯一事实来源

内存消息、TUI 消息和 `meta.json` 都不能成为独立事实来源。进程启动时必须能够仅依赖 `events.jsonl` 重建：

- session metadata；
- 完整 transcript；
- 当前 context epoch；
- compaction summary；
- 可发送给模型的 `Vec<ChatMessage>`；
- turn 数量和最终状态。

### 3.3 Session 是深模块

Session 模块通过较小 interface 隐藏以下实现复杂度：

- 本地路径发现；
- 文件权限；
- 文件锁；
- JSONL 编解码；
- schema version；
- sequence 分配；
- 写入和 flush 顺序；
- torn tail 修复；
- metadata 重建；
- transcript/context projection；
- tool-call 完整性校验；
- reset 和 compaction cutoff；
- active draft 恢复。

当前只有文件系统一种存储实现，不增加 `SessionStore` trait。测试通过 `tempdir` 使用同一个实现。出现第二种真实 adapter 后再抽象 seam。

## 4. 术语

| 术语 | 含义 |
| --- | --- |
| Session | 一段可跨进程恢复的长期交互记录 |
| Runtime run | 一次 Noya 进程打开并使用 session 的生命周期 |
| Turn | 一次用户输入以及由它触发的全部模型和工具循环 |
| Transcript | 用于用户查看和导出的完整记录 |
| Context projection | 从日志投影出的模型请求上下文 |
| Context epoch | `/reset` 后开始的一段新上下文区间 |
| Compaction | 用 summary 替换早期上下文，但不删除原始日志 |
| Durable event | 已追加到 JSONL 并满足对应 flush 要求的语义事件 |
| Active draft | 尚未形成完整 assistant response 的流式部分文本 |

## 5. 模块结构

```text
src/
  session/
    mod.rs          外部 interface 和模块文档
    model.rs        SessionId、SessionMeta、SessionSummary、筛选条件
    event.rs        EventEnvelope、SessionEvent 和 schema version
    filesystem.rs   路径、权限、锁、append、flush、原子文件更新
    projection.rs   transcript、metadata 和 model context reducer
    recovery.rs     torn tail、active draft 和未完成 turn 恢复
    compaction.rs   summary 请求、cutoff 选择和压缩策略
```

职责划分：

```text
CLI/TUI
  -> SessionManager：create/open/list/archive
  -> Agent：submit/cancel/reset/compact

Agent
  -> Session：持久化 turn 语义并读取 context projection
  -> LlmClient：执行模型请求
  -> ToolRegistry：执行工具

Session
  -> Filesystem implementation：本地 JSONL 和派生文件
```

TUI 不直接写 session 文件。Filesystem implementation 不依赖 TUI、LLM 或 Tool。

## 6. 外部 Interface

### 6.1 SessionManager

```rust
pub struct SessionManager {
    root: PathBuf,
}

impl SessionManager {
    pub fn discover() -> Result<Self>;
    pub fn at(root: impl Into<PathBuf>) -> Self;

    pub fn create(&self, options: CreateSession) -> Result<Session>;
    pub fn open(&self, id: SessionId) -> Result<Session>;
    pub fn latest(&self, workspace: &Path) -> Result<Option<SessionSummary>>;
    pub fn list(&self, filter: SessionFilter) -> Result<Vec<SessionSummary>>;
    pub fn archive(&self, id: SessionId) -> Result<()>;
    pub fn export(&self, id: SessionId, format: ExportFormat) -> Result<String>;
}
```

`SessionManager` 负责 session 生命周期，不负责运行 turn。

### 6.2 Session

对 CLI/TUI 公开的 interface 保持只读和生命周期导向：

```rust
impl Session {
    pub fn id(&self) -> &SessionId;
    pub fn summary(&self) -> SessionSummary;
    pub fn transcript(&self) -> Transcript;
    pub fn context(&self) -> ModelContext;
}
```

Agent 使用的 mutation interface 保持 `pub(crate)`：

```rust
impl Session {
    pub(crate) fn start_runtime(&mut self, snapshot: RuntimeSnapshot) -> Result<RunId>;
    pub(crate) fn begin_turn(&mut self, input: String) -> Result<TurnId>;
    pub(crate) fn record_assistant(&mut self, response: AssistantRecord) -> Result<()>;
    pub(crate) fn record_tool_started(&mut self, call: ToolCallRecord) -> Result<()>;
    pub(crate) fn record_tool_finished(&mut self, result: ToolResultRecord) -> Result<()>;
    pub(crate) fn finish_turn(&mut self, turn_id: &TurnId) -> Result<()>;
    pub(crate) fn fail_turn(&mut self, turn_id: &TurnId, failure: TurnFailure) -> Result<()>;
    pub(crate) fn reset_context(&mut self) -> Result<()>;
    pub(crate) fn apply_compaction(&mut self, compaction: CompactionRecord) -> Result<()>;
}
```

这些方法内部必须先完成 durable append，再更新内存 projection，避免磁盘状态落后于内存状态。

## 7. 本地目录结构

根目录解析顺序：

1. `NOYA_DATA_DIR`；
2. `dirs::data_local_dir()/noya`；
3. 无法确定本地数据目录时返回明确错误，不回退临时目录。

```text
<noya-data>/
  sessions/
    <session-uuid>/
      meta.json
      events.jsonl
      active.json
      session.lock
  archive/
    <session-uuid>/
      ...
```

文件职责：

| 文件 | 是否为事实来源 | 行为 |
| --- | --- | --- |
| `events.jsonl` | 是 | 只追加，不原地重写 |
| `meta.json` | 否 | 原子替换，可从事件重建 |
| `active.json` | 否 | 流式生成期间原子替换，完成后删除 |
| `session.lock` | 否 | advisory lock，进程结束后自动释放 |

Unix 权限：

- `<noya-data>`、`sessions/` 和 session 目录：`0700`；
- `events.jsonl`、`meta.json`、`active.json` 和 `session.lock`：`0600`。

## 8. Session metadata

`meta.json` 用于快速列表，不参与模型上下文恢复：

```json
{
  "schema_version": 1,
  "session_id": "019f...",
  "title": "Implement session persistence",
  "workspace": "/absolute/path/to/repo",
  "model": "qwen",
  "model_id": "qwen3-coder-plus",
  "created_at": "2026-08-02T10:30:00Z",
  "updated_at": "2026-08-02T11:10:00Z",
  "status": "idle",
  "last_seq": 42,
  "completed_turns": 3,
  "context_epoch": 1,
  "parent_session_id": null,
  "archived": false
}
```

标题默认取第一条用户输入压缩空白后的前 60 个 Unicode 字符。用户可以显式重命名。

列表按 `updated_at` 降序排列。`meta.json` 缺失或过期时，打开 session 必须重放日志并修复；列表命令可以标记该 session 需要 repair，但不能把它静默隐藏。

## 9. Durable event schema

每一行是一个完整 envelope：

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub schema_version: u32,
    pub seq: u64,
    pub event_id: Uuid,
    pub session_id: SessionId,
    pub run_id: Option<RunId>,
    pub turn_id: Option<TurnId>,
    pub timestamp: OffsetDateTime,
    pub event: SessionEvent,
}
```

约束：

- `schema_version` 第一版固定为 `1`；
- `seq` 从 `1` 开始严格递增；
- `event_id` 全局唯一；
- session 内排序只依赖 `seq`，不依赖 wall clock；
- `run_id` 标识一次进程使用周期；
- turn 相关事件必须包含 `turn_id`；
- JSON 使用紧凑单行编码，以换行符结束。

### 9.1 SessionEvent

```rust
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum SessionEvent {
    SessionCreated(SessionCreated),
    RuntimeStarted(RuntimeSnapshot),
    TitleChanged { title: String },
    ModelChanged(ModelSnapshot),

    TurnStarted(UserMessageRecord),
    AssistantCompleted(AssistantRecord),
    ToolStarted(ToolCallRecord),
    ToolFinished(ToolResultRecord),
    TurnCompleted,
    TurnFailed(TurnFailure),
    TurnCancelled { reason: String },
    TurnInterrupted { reason: String, partial_output: Option<String> },

    ContextReset { new_epoch: u64 },
    ContextCompacted(CompactionRecord),
    SessionForked { parent_session_id: SessionId, through_seq: u64 },
    SessionArchived,
}
```

第一版不使用无约束的 generic event。新增语义必须增加正式 variant、projection 行为和兼容性测试。

### 9.2 AssistantRecord

```rust
pub struct AssistantRecord {
    pub message_id: Uuid,
    pub content: String,
    pub reasoning_content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}
```

`tool_calls` 必须完整保存：

```rust
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}
```

不能只保存工具名、参数或最终 tool result。恢复 OpenAI-compatible history 时需要原始 assistant `tool_calls` 和对应 `tool_call_id`。

### 9.3 ToolResultRecord

```rust
pub struct ToolResultRecord {
    pub call_id: String,
    pub name: String,
    pub result: serde_json::Value,
    pub success: bool,
    pub duration_ms: u64,
}
```

进入日志的是已经经过 `max_tool_output_bytes` 限制的结果。日志不能绕过 Agent 当前的 tool result 上限保存无限 payload。

## 10. Turn 不变量

1. 同一个 session 同时最多有一个 active turn。
2. `TurnStarted` 必须在第一次 LLM 请求前 durable。
3. `TurnStarted` 同时包含完整用户消息，不能分成两个可能部分成功的事件。
4. 每个 `AssistantCompleted.tool_calls` 中的 call ID 在该 turn 内唯一。
5. 每个 `ToolStarted` 必须对应之前出现的 assistant tool call。
6. 每个 `ToolFinished` 必须对应同 call ID 的 `ToolStarted`。
7. 一个 assistant tool-call group 中的全部调用得到 tool result 后，才能发起下一次 LLM completion。
8. `TurnCompleted` 只能出现在不再存在 unresolved tool call 时。
9. `TurnFailed`、`TurnCancelled` 和 `TurnInterrupted` 是互斥 terminal event。
10. terminal event 之后不能再追加同一 turn 的 assistant 或 tool event。

如果取消发生在 assistant 已声明多个 tool call、但部分工具尚未执行时，Agent 必须为未完成 call 记录结构化 cancelled tool result，或将整个 turn 标记为 cancelled 并从后续 context projection 排除。第一版选择后者，避免声称未执行工具已经产生结果。

## 11. 写入顺序与 durability

### 11.1 正常 turn

```text
append TurnStarted
sync_data
  -> LLM completion

append AssistantCompleted
flush

for each tool call:
  append ToolStarted
  sync_data
    -> execute tool
  append ToolFinished
  sync_data

  -> next LLM completion

append final AssistantCompleted
append TurnCompleted
sync_data
update meta.json atomically
```

`flush` 只保证用户空间缓冲写入操作系统；关键语义边界使用 `sync_data`。

### 11.2 Write-ahead projection

所有 mutation 遵循：

```text
validate event
-> append durable event
-> update in-memory projections
-> emit AgentEvent to TUI
```

不能先更新 `Vec<ChatMessage>`，再 best-effort 写日志。

### 11.3 持久化失败

- `TurnStarted` 写入失败：不调用模型；
- `AssistantCompleted` 写入失败：停止 turn，不执行其中的工具；
- `ToolStarted` 写入失败：不执行工具；
- `ToolFinished` 写入失败：不发起下一次模型请求；
- `TurnCompleted` 写入失败：向 TUI 报告 session persistence error，恢复时按 interrupted turn 处理；
- `meta.json` 更新失败：不影响已经 durable 的事件，但要显示 recoverable warning。

## 12. Context projection

Session 维护两个 projection。

### 12.1 TranscriptProjection

用于 TUI、`session show` 和 Markdown 导出，包含：

- 用户消息；
- 完整和中断的 assistant 消息；
- tool started/finished 状态；
- turn error、cancel 和 interruption；
- reset、compaction 和 model change 分隔信息。

Transcript 不因 reset 或 compaction 丢弃原始记录。

### 12.2 ModelContextProjection

用于生成 `Vec<ChatMessage>`。包含：

```text
当前 system prompt
+ 可选 compaction summary
+ 当前 context epoch 中、cutoff 之后的 completed turns
+ 当前进程正在执行的 active turn
```

规则：

- 默认只恢复具有 `TurnCompleted` 的 turn；
- failed、cancelled 和 interrupted turn 保留在 transcript，但不进入下一次模型请求；
- assistant tool-call message 与它的全部 tool result 作为不可拆分 group；
- 发现孤立 tool result、重复 call ID 或缺失 tool result 时，session 标记为 corrupt；
- system prompt 不作为普通历史消息反复追加；
- `reasoning_content` 按模型兼容要求恢复，但不显示在普通 transcript 中；
- projection 的输出必须直接满足 `ChatMessage` 协议，不由 LLM adapter 猜测修复。

## 13. Runtime snapshot

每次进程打开 session 后追加 `RuntimeStarted`：

```rust
pub struct RuntimeSnapshot {
    pub noya_version: String,
    pub workspace: PathBuf,
    pub model: String,
    pub model_id: String,
    pub system_prompt: String,
    pub tool_names: Vec<String>,
    pub max_tool_loops: usize,
    pub tool_timeout_ms: u64,
    pub max_tool_output_bytes: usize,
    pub temperature: Option<f32>,
}
```

不保存：

- API key；
- Authorization header；
- credential 文件内容；
- 带 secret query parameter 的完整 base URL。

恢复 session 时使用当前 workspace 重新构建 system prompt，同时通过历史 snapshot 保留审计能力。

## 14. 流式输出与 active draft

不把每个 `TextDelta` 写入 JSONL。高频 delta 会放大日志并导致大量小 IO。

### 14.1 active.json

收到第一个文本 chunk 后创建：

```json
{
  "schema_version": 1,
  "session_id": "...",
  "run_id": "...",
  "turn_id": "...",
  "message_id": "...",
  "content": "partial assistant output",
  "updated_at": "2026-08-02T10:31:00Z"
}
```

更新策略：

- 至多每 250ms 更新一次；或
- 累计新增内容达到 4 KiB 时立即更新；
- 使用同目录临时文件、flush 和 rename 原子替换；
- assistant 完成并 durable 写入 `AssistantCompleted` 后删除；
- tool call 参数不在流式阶段写入 draft，等完整解析后随 `AssistantCompleted` 一次保存。

`active.json` 写入失败不允许破坏正常流式显示，但 TUI 必须提示 crash recovery checkpoint 不可用。

### 14.2 正常完成

```text
TextDelta -> TUI 实时显示 + 内存 accumulator
            -> active.json 周期 checkpoint

stream end -> append AssistantCompleted
           -> sync/flush
           -> delete active.json
```

## 15. 打开与恢复流程

```text
resolve session directory
-> acquire exclusive lock
-> validate/repair JSONL tail
-> replay durable events
-> validate sequence and invariants
-> compare/rebuild meta.json
-> inspect active.json
-> close unfinished turn as interrupted
-> append RuntimeStarted
-> expose transcript and context projection
```

### 15.1 未完成 turn

日志末尾存在 `TurnStarted`，但没有 terminal event 时：

1. 如果 `active.json` 属于该 turn，读取部分输出；
2. 追加 `TurnInterrupted { reason: "process_terminated", partial_output }`；
3. 删除 `active.json`；
4. 将该 turn 显示在 transcript；
5. 不把该 turn 加入恢复后的模型上下文。

恢复过程不自动重跑 tool call，因为进程可能在工具产生外部副作用后、写入结果前崩溃。

### 15.2 JSONL 损坏策略

- 文件为空：corrupt，除非 session 目录刚创建且没有发布；
- 最后一行没有完整换行且无法解析：截断到上一个有效换行；
- 最后一行有完整换行但无法解析：corrupt；
- 中间任意行无法解析：corrupt；
- sequence 重复、倒退或跳号：corrupt；
- 不支持的未来 schema version：返回 upgrade-required 错误；
- 未知 event variant：返回 upgrade-required 错误；
- 不允许静默跳过损坏事件。

修复 torn tail 前先创建同目录 `.repair` 备份，修复成功后可以删除备份。

## 16. 文件锁和并发

打开可写 session 时对 `session.lock` 获取 exclusive advisory lock，并在 `Session` 生命周期内持有文件 handle。

- 锁冲突时返回 `session is already open by another Noya process`；
- 不依赖 PID 文件判断进程是否存活；
- 进程退出或崩溃后由操作系统释放 advisory lock；
- `sessions`、`session show` 和 export 可以只读打开日志；
- 只读命令不能修复日志或更新 metadata，除非显式传入 repair；
- TUI 同一时刻只运行一个 turn，沿用现有 Agent host 串行模型。

## 17. Session 操作语义

### 17.1 New

裸 `noya` 创建新 session。创建顺序：

1. 生成 UUID；
2. 创建权限为 `0700` 的 session 目录；
3. 获取 lock；
4. 创建 `events.jsonl`；
5. 写入并 sync `SessionCreated`；
6. 原子写入 `meta.json`；
7. 写入 `RuntimeStarted`。

### 17.2 Resume

`noya resume` 恢复当前 workspace 最近更新的非归档 session。

`noya resume <id-prefix>` 恢复唯一匹配的 session。前缀匹配到多个 session 时必须报错并列出候选项。

Workspace 规则：

- 未显式传 `--workspace`：使用 session 记录的 workspace；
- 显式 workspace 与记录不一致：拒绝打开；
- 第一版不支持 workspace rebind。

模型解析优先级：

```text
显式 --model/--model-id
-> session 最近的 model snapshot
-> 当前 active model 配置
```

模型变化时追加 `ModelChanged`，历史消息保持不变。

### 17.3 Clear

`/clear` 只清空 TUI 当前显示 projection：

- 不写 session event；
- 不改变模型上下文；
- 不删除 terminal native scrollback；
- 下一次进程恢复仍能看到完整 transcript。

### 17.4 Reset

`/reset` 在 session idle 时追加：

```text
ContextReset { new_epoch: previous_epoch + 1 }
```

行为：

- transcript 保留 reset 前内容并显示分隔线；
- 模型上下文只包含新 epoch 之后的 turns；
- session ID 和日志文件不变；
- reset 清除当前 compaction summary；
- reset 不等于创建新 session。

### 17.5 Retry

`/retry` 找到最近一个 failed、cancelled 或 interrupted turn 的用户输入，并以新的 turn ID 重新提交。

- 原 turn 不修改；
- retry 是一个普通新 turn；
- TUI 显示它与原 turn 的关联；
- 第一版关联信息可以只存在 transcript projection，不必进入模型消息。

### 17.6 Rename

`/rename <title>` 追加 `TitleChanged`。空标题、控制字符或超过 120 个 Unicode 字符时拒绝。

### 17.7 Archive

归档顺序：

1. 确认 session 未被可写打开；
2. 追加并 sync `SessionArchived`；
3. 更新 metadata；
4. 原子移动整个目录到 `archive/`。

归档 session 不参与默认 `resume` 和 `sessions`，但可以通过 `--archived` 查看和导出。

物理删除不属于第一阶段。

### 17.8 Fork

Fork 属于第二阶段。它创建新 session，并复制父 session 截止某个 completed turn 的有效语义记录。

- 新 session 使用新 ID、新 sequence 和新 event ID；
- `SessionCreated` 保存 `parent_session_id`；
- `SessionForked` 保存父 session 和 `through_seq`；
- 不复制 active、failed、cancelled 或 interrupted turn；
- fork 完成后不依赖父 session 才能恢复。

不实现原地 rollback。需要回到旧 turn 时使用 fork，避免在单个 append-only session 内引入上下文分支图。

## 18. Compaction

### 18.1 目标

Compaction 只减少模型上下文，不删除 transcript 和 JSONL 历史。

```rust
pub struct CompactionRecord {
    pub summary: String,
    pub through_seq: u64,
    pub through_turn_id: TurnId,
    pub keep_from_turn_id: Option<TurnId>,
    pub source_token_estimate: usize,
    pub summary_model: String,
}
```

### 18.2 Cutoff 规则

- 只压缩 completed turns；
- cutoff 必须位于 turn 边界；
- 保留最近至少 4 个 completed turns；
- 不能拆分 assistant tool-call group；
- reset 后只压缩当前 epoch；
- compaction summary 生成失败时不改变 context；
- `ContextCompacted` durable 后才能替换内存 context projection。

### 18.3 Model context

压缩后的上下文：

```text
system prompt
+ context summary message
+ keep_from_turn_id 开始的完整 turns
```

如果后续再次压缩，新 summary 输入必须包含旧 summary 和从旧 cutoff 之后选中的 turns。

### 18.4 触发方式

第一阶段支持手动 `/compact`。

自动 compaction 在 model catalog 提供可靠 context window 后启用：

- estimated tokens 达到 context window 的 75% 时触发；
- 未知 context window 时不自动触发；
- 当前 turn 执行期间不触发；
- 用户可以通过配置关闭自动 compaction。

## 19. CLI 设计

```text
noya
noya resume [session-id-prefix]
noya sessions [--all] [--archived] [--json]
noya session show <id> [--json]
noya session export <id> [--format markdown|jsonl]
noya session archive <id>
noya session tree <id> [--json]
noya session branch-create <id> <name> [--from-seq <seq>]
noya session branch-select <id> <branch-uuid> [--summary <text>]
```

全局参数：

```text
--workspace <path>
--model <model>
--model-id <id>
```

为了区分默认 workspace 和用户显式指定的 workspace，CLI 字段应从当前的默认 `PathBuf(".")` 改为 `Option<PathBuf>`，在命令语义层解析默认值。

`sessions` 默认只列出当前 workspace 的非归档 session：

```text
SESSION       UPDATED              MODEL       TURNS  TITLE
019fbd63...   2026-08-02 11:10     qwen        3      Implement session persistence
```

## 20. TUI 设计

新增命令：

```text
/new
/sessions
/resume <id-prefix>
/rename <title>
/retry
/compact
/tree
/branch <name>
/branch select <branch-id-prefix> [summary]
```

更新命令语义：

```text
/clear      只清空显示
/reset      建立新 context epoch
/status     增加 session 信息
```

`/status` 显示：

- session ID；
- title；
- session log path；
- workspace；
- model 和 model ID；
- completed turns；
- context epoch；
- estimated context tokens；
- compaction cutoff；
- Agent state。

启动恢复 session 时：

- 默认向 TUI 投影最近 200 条 transcript item；
- 显示更早记录数量；
- 完整历史仍可通过 `session show/export` 获取；
- 历史投影完成后再显示 `Noya ready`；
- 不把历史消息重新作为新的 durable event 写入日志。

Agent busy 时禁止 `/new`、`/resume`、`/reset`、`/compact` 和 archive。用户必须先等待或 `/cancel`。

## 21. Agent 集成

当前 `Agent.messages: Vec<ChatMessage>` 应被 Session projection 替代。

目标结构：

```rust
pub struct Agent {
    config: AgentConfig,
    llm: LlmClient,
    tools: ToolRegistry,
    session: Session,
}
```

每次 completion 前：

```rust
let messages = self.session.context().messages;
```

AgentEvent 继续用于实时 UI，不作为 durable session interface。需要为事件增加稳定 ID：

```rust
pub enum AgentEvent {
    TurnStarted { turn_id: TurnId },
    TextDelta { turn_id: TurnId, message_id: Uuid, chunk: String, is_final: bool },
    ToolStarted { turn_id: TurnId, call_id: String, name: String, arguments: Value },
    ToolFinished { turn_id: TurnId, call_id: String, name: String, result: Value, success: bool },
    TurnCompleted { turn_id: TurnId },
    Error { turn_id: Option<TurnId>, message: String, recoverable: bool },
}
```

TUI 使用这些 ID 合并流式消息。Session 使用自己的正式 mutation 方法写 durable events，不通过观察 AgentEvent 猜测持久化语义。

## 22. Schema evolution

- 每个 envelope 带 `schema_version`；
- 第一版 loader 只接受 version `1`；
- 升级通过纯函数 `migrate_vN_to_vN_plus_1` 在内存中转换；
- 默认不原地重写旧日志；
- 如需要物理迁移，先写新文件、校验完整重放，再原子替换并保留备份；
- 新增 optional 字段必须有 serde default；
- 删除或改变既有字段语义必须提升 schema version；
- JSONL export 始终输出原始 durable events，不输出内存 projection。

## 23. 安全与隐私

Session 可能包含用户代码、命令输出和模型 reasoning，属于敏感本地数据。

必须满足：

- API key 永不进入 event、metadata、active draft 或 tracing；
- base URL 只保存安全的 origin/path，不保存 userinfo 和 secret query；
- tool argument/result 按现有上限持久化；
- credential 文件和 session 数据使用独立目录；
- export 默认输出到 stdout，不自动写入 workspace；
- 错误信息不得包含 bearer token；
- archive 不降低文件权限；
- 文档明确说明 session 日志可能包含源码和命令输出。

第一版不实现内容级自动 secret redaction，因为通用 redaction 可能破坏代码和工具结果；后续可增加 opt-in redaction policy。

## 24. 实施阶段

### Phase 1：Session domain 和文件系统

- 新建 `src/session/`；
- 实现 ID、metadata 和 event schema；
- 实现目录发现、权限和 advisory lock；
- 实现 append、flush、sync 和 atomic metadata；
- 实现 transcript/context/metadata projection；
- 实现 torn tail 和 corruption 检测；
- 使用 `tempdir` 完成 interface-level 测试。

### Phase 2：Agent 持久化集成

- `Agent` 持有 `Session`；
- 移除独立 `messages` 事实来源；
- 按 write-ahead 顺序记录 user、assistant 和 tool；
- 增加稳定 run/turn/message ID；
- 完成 failed、cancelled、interrupted turn 语义；
- 验证 tool-call history 恢复后可以直接发送给模型。

### Phase 3：流式恢复与 TUI

- 实现 `active.json` checkpoint；
- 恢复部分流式回答；
- 为 TUI 注入历史 transcript；
- 实现 `/new`、`/sessions`、`/resume`、`/rename`、`/retry`；
- 更新 `/reset`、`/clear` 和 `/status`；
- 验证取消和 session 切换。

### Phase 4：CLI session 管理

- 实现 `resume`、`sessions` 和 `session show/export/archive`；
- 支持 ID prefix；
- 支持 workspace 和 model 恢复规则；
- 支持 Markdown 与原始 JSONL 导出；
- 更新 README 和 README-ZH。

### Phase 5：Compaction 和 fork

- 实现手动 compaction；
- 增加 context token estimate 和 model context window；
- 实现自动 compaction；
- 实现完整、独立的 session fork；
- 验证多次 compaction 和 fork 后的上下文一致性。

## 25. 测试策略

### 25.1 Session interface 测试

- create 后能够 open；
- append 后重启能够恢复相同 transcript；
- metadata 能从日志完全重建；
- session list 按 workspace 和 updated time 过滤排序；
- archive 后默认列表和 latest 不再返回；
- prefix 唯一匹配和冲突行为正确。

### 25.2 Turn 和 tool 不变量测试

- 普通 user/assistant turn；
- 一个 assistant 发起一个工具；
- 一个 assistant 发起多个工具；
- tool timeout 和失败结果；
- assistant tool call 缺少 tool result 时拒绝完成；
- duplicate call ID 标记 corrupt；
- cancel 发生在 LLM、工具和工具组之间；
- failed/cancelled/interrupted turn 不进入恢复 context。

### 25.3 文件故障测试

- 最后一行半写自动修复；
- 中间损坏拒绝打开；
- sequence 重复、倒退和跳号拒绝打开；
- stale/missing `meta.json` 重建；
- stale `active.json` 转为 interrupted turn；
- metadata rename 失败不破坏 durable log；
- 两个 writer 同时打开时第二个失败；
- session 文件权限符合要求。

### 25.4 Compaction 测试

- compaction 后原始 transcript 不变；
- 恢复后不会重新引入 cutoff 前原始消息；
- summary 只出现一次；
- 不拆分 assistant/tool group；
- reset 清除旧 compaction projection；
- 连续多次 compaction 可重放；
- compaction 失败不改变 context。

### 25.5 集成测试

- 使用假的 SSE 模型完成 turn、退出、resume，再完成下一 turn；
- 恢复后的请求体包含完整 assistant `tool_calls` 和 tool `tool_call_id`；
- 流式中断后 transcript 显示部分输出；
- TUI `/clear` 不影响重启后的历史；
- TUI `/reset` 重启后仍维持 context epoch；
- credential/API key 不出现在 session 目录内容中。

## 26. 验收标准

完整实现必须满足：

1. 裸 `noya` 创建新的本地 session；
2. `noya resume` 能恢复当前 workspace 最近 session；
3. 可以使用 ID prefix 打开唯一 session；
4. 重启后 transcript、model context、tool-call 对应关系完全恢复；
5. 流式回答实时显示，进程中断后保留最近 checkpoint；
6. 未完成 turn 不污染后续模型上下文；
7. tool result 永远有对应 assistant tool call 和 call ID；
8. `/clear` 不改变日志或模型上下文；
9. `/reset` 建立 durable context epoch，重启后语义不变；
10. compaction 不删除原始日志，重启后不重新引入已压缩历史；
11. session 并发写入被文件锁拒绝；
12. torn tail 可以修复，中间损坏不会被静默忽略；
13. metadata 丢失时可以从 JSONL 重建；
14. session 可列出、查看、重命名、归档和导出；
15. API key 和 Authorization 信息不进入任何 session 文件；
16. 不依赖 SQLite 或其他数据库；
17. `cargo fmt --check`、`cargo check`、`cargo test` 和严格 clippy 全部通过。

## 27. 推荐依赖变化

```toml
time = { version = "0.3", features = ["formatting", "parsing", "serde"] }
fs2 = "0.4"
```

`uuid` 增加 `serde` 支持；如果采用 UUID v7，则同时增加 `v7` feature。除非后续需要 workspace fingerprint，不增加 hash 依赖。

## 28. 最终边界

Session 模块保证：

- 本地 durable history；
- 可验证重放；
- provider-valid model context；
- reset/compaction 的持久语义；
- 单 writer；
- 明确的中断和损坏行为。

Session 模块不保证：

- tool 外部副作用 exactly-once；
- 远程同步；
- transport delta replay；
- 自动 secret detection；
- 多写入者合并。

这些边界必须在实现、CLI 错误信息和用户文档中保持一致。
