---
status: accepted
---

# Discover and cache provider model catalogs

Noya will keep discovered model IDs in a separate `models.json` beside `credentials.json` (under `NOYA_CONFIG_DIR`, defaulting to `~/.noya`). A Model Catalog is identified by `provider + normalized_base_url`; API keys are never written to the catalog or included in its identity. Each catalog stores only its model IDs, discovery timestamp, provider, and normalized base URL.

At startup, Noya will load the cache immediately and asynchronously run Model Discovery for configured providers with credentials and a valid base URL. A newly completed provider setup triggers discovery immediately. There is no manual refresh command. Successful discovery replaces that provider's cache; a failed discovery keeps the old cache without interrupting startup, and a provider with no cache exposes no selectable models.

If the configured model ID disappears from a refreshed catalog, Noya will switch automatically to the provider default model when present, otherwise to the first catalog entry, and persist the new model ID without prompting the user.

## Considered options

- Store catalogs in `credentials.json`: rejected because model catalogs are non-secret data and have a different lifecycle from credentials.
- Require manual refresh: rejected because model IDs are long and users should not need to remember provider-specific model identifiers or maintenance commands.
- Preserve a removed model indefinitely: rejected because it leaves the runtime pointed at a model the provider no longer advertises.
