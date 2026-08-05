# Make Provider Protocol and Authentication Explicit

Noya will model the Provider Protocol and Authentication Mode separately instead of assuming every provider uses OpenAI-compatible requests or inferring behavior from a base URL. Claude defaults to the native Anthropic Messages protocol, while OpenAI-compatible providers remain available explicitly; this supports both the local Anthropic-compatible service and the official Anthropic API without sacrificing OpenRouter-style gateways.

## Consequences

- Model discovery remains provider-specific but keeps the shared model catalog shape where the endpoint already returns it.
- Anthropic integrations can use native `/messages` streaming and tool-use semantics.
- A provider configuration must identify both its protocol and authentication mode.
