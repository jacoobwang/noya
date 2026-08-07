# Noya Model Configuration Context

This context defines the language used for configuring providers and discovering the models they expose.

## Model Configuration

**Provider**:
A named model-service boundary that supplies credentials, a base URL, and access to one or more models through a declared provider protocol. Examples include OpenAI, DeepSeek, Qwen, Kimi, and Claude.
_Avoid_: Model, vendor

**Model ID**:
The exact identifier understood by a provider for one concrete model, such as `claude-sonnet-5`.
_Avoid_: Model name, provider

**Model Catalog**:
The set of Model IDs exposed by a specific Provider endpoint at discovery time. Its identity is the Provider plus the normalized base URL; credentials are not part of the identity.
_Avoid_: Credentials, model configuration

**Model Discovery**:
The process of obtaining a Provider's current Model Catalog from its model-list endpoint.
_Avoid_: Model configuration, credential refresh

**Provider Protocol**:
The wire contract a Provider uses for model discovery, conversation requests, streaming, and tool calls. Noya supports OpenAI-compatible and Anthropic Messages protocols as distinct contracts.
_Avoid_: Model, endpoint

**Authentication Mode**:
The request-header convention used to present a Provider credential, such as Bearer authentication or the Anthropic `x-api-key` header.
_Avoid_: API key, Provider Protocol

**Anthropic Messages Protocol**:
The native Anthropic conversation contract centered on `/messages`, including Anthropic content blocks, streaming events, and tool-use turns.
_Avoid_: OpenAI-compatible protocol, Claude model

**Model Selection Fallback**:
The deterministic automatic choice used when a configured Model ID is no longer present in the latest Model Catalog: prefer the Provider default, then the first catalog entry.
_Avoid_: Silent model migration, random model selection

## Project Workers

**Project**:
A canonicalized, existing local directory that Noya uses as a coding workspace. Projects are identified by their directory path, not by a user-defined name.
_Avoid_: Repository, session

**Worker**:
The long-lived runtime agent bound to exactly one Project. A Worker owns that Project's model, active Skills, session execution, tool calls, and approval state, and executes Jobs sequentially for its bound Session context.
_Avoid_: Thread, session, workspace process

**Job**:
A user-submitted unit of work executed by a Worker. A Job has its own lifecycle, request identity, outcome, and event cursor, while using a Session as its durable conversation context.
_Avoid_: Worker, turn, session

**Job Manager**:
The orchestration boundary that accepts Jobs, assigns them to Workers, tracks their lifecycle, and exposes status, cancellation, result, and reconnect behavior.
_Avoid_: Worker registry, session manager

**Job Lifecycle**:
The states a Job can occupy: queued before assignment, running while the Worker executes it, waiting approval when a tool requires user authorization, and completed, failed, cancelled, or interrupted as terminal outcomes.
_Avoid_: Session status, Agent turn status

**Interrupted Job**:
A Job whose Worker stopped before a terminal outcome because the daemon exited, crashed, or lost its runtime. An Interrupted Job is not automatically replayed; retry is an explicit new execution decision.
_Avoid_: Failed Job, cancelled Job

**Job Record**:
The durable identity and lifecycle history of a Job, including its request, Session reference, Project, state transitions, outcome, and reconnect cursor. A Job Record is separate from the Session's conversation history.
_Avoid_: Session record, transcript

**Job Scheduling**:
The bounded assignment of queued Jobs to Workers. A Worker executes at most one Job for its Session at a time; Jobs for different Projects or Sessions may run concurrently up to the configured Worker capacity.
_Avoid_: Parallel turns, unbounded background execution

**Job Approval**:
The explicit authorization of a tool call requested by a Job. A Job that needs authorization remains waiting approval until a client approves or rejects it; client disconnection does not authorize the call.
_Avoid_: Automatic approval, interactive turn approval

**Cancelling Job**:
A running Job for which cancellation has been requested but the Worker has not yet returned. Cancellation is cooperative; the Job becomes cancelled only after the Worker acknowledges the stop.
_Avoid_: Interrupted Job, force-killed Job

**Job Retry**:
An explicit new Job created from a failed, interrupted, or cancelled Job. It receives a new identity, keeps a reference to the original Job, and reuses the original Project and Session context.
_Avoid_: In-place restart, automatic replay

**Job Session**:
The durable Session context assigned to a Job. A Job Session is independently writable by its Worker so a background Job does not concurrently write the Session currently owned by an interactive client.
_Avoid_: Active TUI session, shared transcript

**Job Recovery**:
The explicit restart policy for durable Job Records: queued Jobs remain eligible for scheduling, while running, waiting approval, or cancelling Jobs become interrupted and require an explicit retry.
_Avoid_: Automatic turn replay, transparent continuation

**Job Event Stream**:
The ordered lifecycle and Agent-event history of a Job. Its durable sequence is the source of truth for reconnect; the daemon's in-memory broadcast and bounded replay are only a live-delivery optimization.
_Avoid_: Session transcript, socket buffer

**Worker Reuse**:
The lifecycle policy for the runtime that executes Jobs: a Worker may be reused for later queued Jobs targeting the same Project and Job Session, while the Job Record remains the unit of execution history.
_Avoid_: Job identity reuse, shared concurrent session writer

**Job Observation**:
The client view of a Job's state and events without changing the active interactive Session. Observing a Job does not merge its transcript into the active Session.
_Avoid_: Session switch, transcript merge

**Active Worker**:
The Worker currently connected to the TUI input and transcript view. Other Workers may continue turns in the background but do not inject their output into the active view.
_Avoid_: Current session

**Project Switch**:
Changing the Active Worker to another Project, loading that Project's latest non-archived Session, or creating a new Session when none exists.
_Avoid_: Workspace rebind, session migration

**Project History**:
The list of distinct Projects derived from Session metadata and ordered by each Project's latest Session update. The TUI exposes at most ten entries.
_Avoid_: Project database, worker registry

## Code Navigation

**Language Server**:
A workspace-aware service that understands a programming language and answers semantic code queries such as definitions, references, and symbols.
_Avoid_: Compiler, text search engine

**Code Navigation**:
A semantic query over workspace code that returns definitions, references, or workspace symbols with source locations.
_Avoid_: Text search, file search

**Semantic Result**:
A code-navigation result identified by a Language Server as a definition, reference, or symbol, including its source location and code context.
_Avoid_: Text match, grep result

**Text Search Fallback**:
A plain-text search result used when workspace-symbol navigation has no available Language Server; it is not treated as a semantic definition or reference.
_Avoid_: Semantic fallback, inferred definition

## Skills

**Skill**:
A named package of Markdown instructions and repository-local resources that changes how an agent works without adding executable authority.
_Avoid_: Tool, plugin, prompt template

**Skill Package**:
A directory containing a required SKILL.md and optional resources such as references, scripts, and assets.
_Avoid_: Extension, executable plugin

**Skill Activation**:
The explicit, session-scoped decision to include a Skill's instructions in the agent system prompt.
_Avoid_: Skill discovery, automatic loading

**Skill Discovery**:
The process of scanning the workspace and user Skill roots for valid Skill Packages without activating them.
_Avoid_: Skill activation, plugin installation

**Active Skill Set**:
The ordered set of Skills currently included in a Session's system prompt.
_Avoid_: Installed skills, tool registry
