---
status: accepted
---

# Use bounded background Workers for project switching

Noya models each canonicalized workspace as a Project and binds each runtime Worker to exactly one Project. Multiple Workers may continue turns concurrently, but the process has one Active Worker for TUI input, transcript rendering, approval, and cancellation. The default limit is four Workers and can be configured with `--max-workers` or `NOYA_MAX_WORKERS`.

Switching a Project never rebinds a Session to a different workspace. It activates an existing Worker or opens the target Project's latest non-archived Session, creating a new Session when necessary. Inactive Worker events remain isolated and are represented by status/notification state until the Worker becomes active.

## Considered options

- Keep only one Worker and reopen Sessions on every switch: rejected because background turns must continue.
- Rebind one Session's workspace at runtime: rejected because durable transcripts, tools, prompts, and recovery metadata would no longer describe one stable workspace.
- Keep an unbounded Worker for every Project: rejected because each Worker holds session locks, model/runtime state, and asynchronous event/approval channels.
- Run Workers across process restarts: rejected for the first version; Workers are runtime state and interrupted Sessions remain recoverable with `/retry`.
