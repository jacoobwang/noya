---
status: accepted
---

# Use local language servers for semantic code navigation

Noya will add a `code_navigation` tool backed by locally installed Language Servers for definitions, references, and workspace symbols. It will lazily manage one server per workspace and language, use the current Noya workspace as the root, and keep `search_text` as the plain-text path; only workspace-symbol queries may fall back to `rg`, while definition and reference queries must not be presented as semantic results when LSP is unavailable. Local server discovery avoids bundling or installing language tooling, and bounded timeouts plus one lazy restart keep a broken server from blocking an agent turn.
