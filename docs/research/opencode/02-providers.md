# Providers and models

|                     |                                            |
| ------------------- | ------------------------------------------ |
| **opencode commit** | `03e67171ab2dc1e7f16e8cebfbc7f778f61b89f0` |
| **Researched**      | 2026-10-07                                 |
| **Sources**         | source · docs                              |

## Summary

Providers are a `provider.<id>` map in the config, merged over the
models.dev catalogue (served from opencode's own mirror). A model is named
`provider/model`, split on the first `/`. A custom OpenAI-compatible
provider gets only the models its config lists: there is no `/models`
discovery, and a provider left with no models is dropped. The TUI picker is
one flat list with a header per provider, favorites and recents first.

## User-facing behaviour

- `provider.<id>` takes `name`, `npm` (the SDK, so the wire protocol),
  `options.baseURL`, `options.apiKey`, an optional `models` map with
  per-model overrides (`name`, `limit`, `cost`, `reasoning`, ...), and
  `whitelist`/`blacklist`. [source]
- `{env:VAR}` anywhere in the config text is replaced before parsing; a
  missing variable becomes an empty string. `{file:path}` reads a file.
  [source]
- `model` and `small_model` are `provider/model`. [docs]
- A provider is enabled when any variable in its catalogue `env` list is
  set, when `/connect` stored a key, or when the config names it.
  `disabled_providers` and `enabled_providers` filter the result. [source]
- The picker shows Favorites, Recent, then one section per provider with
  `opencode` first and the rest by name; within a provider free models
  first, then newest. Search is fuzzy over model name and provider name.
  `ctrl+f` favourites, `ctrl+a` connects a provider. [source]

## Where it lives

```
packages/core/src/
├── v1/config/provider.ts        # ConfigProviderV1.Info: the provider block schema
├── v1/config/config.ts          # model, small_model, provider map, disabled/enabled_providers
└── models-dev.ts                # catalogue fetch, cache file, refresh
packages/opencode/src/
├── config/variable.ts           # {env:VAR} and {file:path} substitution
└── provider/provider.ts         # catalogue + config merge, enabling, SDK construction, defaultModel
packages/tui/src/
├── component/dialog-model.tsx   # the picker
└── context/local.tsx            # recents and favourites in $XDG_STATE_HOME/opencode/model.json
```

## How it works

1. **Catalogue** — `models-dev.ts` loads `$XDG_CACHE_HOME/opencode/models.json`,
   else a snapshot baked in at build time, else fetches
   `https://models.opencode.ai/api.json` (10s timeout, two retries) and
   refreshes it in the background every 60 minutes.
2. **Merge** — `provider.ts` starts from the catalogue entry for each config
   provider id, deep-merges `options`, overrides `name` and `env`, and parses
   each configured model with fallbacks config → catalogue → default. The
   npm package falls back through model override, provider, catalogue, then
   `@ai-sdk/openai-compatible`. A model not in the catalogue defaults to
   `limit.context = 0`.
3. **Enable** — env variables from the catalogue's `env` list, then stored
   keys from `auth.json`, then plugin loaders, then every config provider.
   A provider with zero models after filtering is deleted.
4. **Route** — `parseModel` splits on the first `/`, so
   `openrouter/openai/gpt-5` is provider `openrouter`, model `openai/gpt-5`.
5. **Wire** — `@ai-sdk/openai-compatible` posts to `{baseURL}/chat/completions`
   with `Authorization: Bearer {apiKey}` and `stream_options.include_usage`;
   `@ai-sdk/openai` is forced onto the Responses API instead.
6. **Default model** — `config.model`, else the first valid recent, else the
   first configured provider's best-ranked model.

## Key data types / interfaces

- **Provider block**: `{ name?, npm?, env?: string[], options?: { baseURL?, apiKey?, timeout?, ... }, models?: Record<id, Model>, whitelist?, blacklist? }`.
- **Model override**: `{ id?, name?, limit?: { context, output }, cost?, reasoning?, tool_call?, attachment?, status?, provider?: { npm, api } }`.
- **Picker state**: `$XDG_STATE_HOME/opencode/model.json` holds `{ recent, favorite, variant }`, recents capped at 10.

## What nth takes from it

- The `provider.<id>` map, the `provider/model` id split on the first `/`,
  and one picker list grouped by provider.
- Not the config-only model lists: nth asks each endpoint's `/models` and
  keeps a config list only as an override.
- Not the catalogue cache or the `{env:VAR}` substitution; nth names the
  variable with `api_key_env` and reads it at startup.
