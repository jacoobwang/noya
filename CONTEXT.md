# Noya Model Configuration Context

This context defines the language used for configuring providers and discovering the models they expose.

## Model Configuration

**Provider**:
A named model-service boundary that supplies credentials, a base URL, and access to one or more models. Examples include OpenAI, DeepSeek, Qwen, Kimi, and Claude through an OpenAI-compatible gateway.
_Avoid_: Model, vendor

**Model ID**:
The exact identifier understood by a provider for one concrete model, such as `anthropic/claude-sonnet-4.5`.
_Avoid_: Model name, provider

**Model Catalog**:
The set of Model IDs exposed by a specific Provider endpoint at discovery time. Its identity is the Provider plus the normalized base URL; credentials are not part of the identity.
_Avoid_: Credentials, model configuration

**Model Discovery**:
The process of obtaining a Provider's current Model Catalog from its OpenAI-compatible model-list endpoint.
_Avoid_: Model configuration, credential refresh

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
