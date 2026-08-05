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
