---
status: accepted
---

# Use durable Jobs for daemon-managed long-running work

Noya models a long-running background execution as a Job managed by the resident daemon. A Worker is the long-lived runtime bound to one Project and Job Session; a Job is one submitted unit of work executed by that Worker. CLI and TUI clients submit, observe, approve, cancel, retry, and reconnect to Jobs through the daemon protocol.

Each Job receives an independent Job Session, normally forked from the submitting interactive Session at its latest completed turn. This keeps the active TUI Session's advisory lock and transcript isolated while preserving the conversation context needed by the background task. Job lifecycle records live separately from Session conversation events under the daemon data directory.

Job scheduling is bounded: a Worker executes at most one Job for its Session at a time, different Sessions may run concurrently up to the configured Worker capacity, and idle Workers may be reused or evicted. The lifecycle is `queued`, `running`, `waiting_approval`, `cancelling`, and the terminal states `completed`, `failed`, `cancelled`, or `interrupted`.

Job events are append-only and durable. The persisted Job event sequence is authoritative for reconnect; daemon-memory broadcast and bounded replay are only a live-delivery optimization. On daemon restart, queued Jobs remain eligible for scheduling while running, approval-waiting, and cancelling Jobs become interrupted. Retry always creates a new Job identity and records `retry_of`; it never automatically replays a partially executed Job.

Background Jobs preserve the configured tool approval policy. A Job pauses in `waiting_approval` instead of automatically approving a mutating or dangerous tool, and local clients can approve or reject it. Cancellation is cooperative: queued Jobs cancel immediately, approval-waiting Jobs are rejected and cancelled, and running Jobs transition through `cancelling` until the Worker returns.

## Considered options

- Reuse the active TUI Session for a background Job: rejected because the TUI and daemon would contend for one session lock and could interleave unrelated transcript events.
- Store Job lifecycle in Session `events.jsonl`: rejected because Job scheduling, reconnect, retry, and terminal outcomes have a different lifecycle from conversation projection.
- Replay an interrupted Job automatically: rejected because a crash can occur after an external side effect and before its durable completion event, making automatic replay unsafe.
- Auto-approve background tool calls: rejected because detaching a client must not widen the configured authority of a Job.
- Use an unbounded Worker pool: rejected because each Worker retains model/runtime state, session locks, and asynchronous control channels.
