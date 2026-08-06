---
status: accepted
---

# Use explicit, session-scoped Skills

Noya will discover Skill Packages from the workspace .agents/skills directory and the user's ~/.noya/skills directory, with project Skills taking precedence over user Skills. Skills are activated explicitly, injected into the system prompt in activation order, and recorded as durable Session events with their source and content digest. This keeps prompt context opt-in and auditable while ensuring Skills cannot alter Tool permissions or other runtime safety limits.

## Considered options

- Inject every discovered Skill at startup: rejected because it consumes context and gives unrequested instructions authority over the agent.
- Read ~/.codex/skills: rejected because it couples Noya to a host application's private filesystem.
- Treat Skills as executable plugins: rejected because prompt guidance must not gain additional tool or sandbox authority.
