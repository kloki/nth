# opencode research

Reference: https://github.com/anomalyco/opencode

|                   |                                                        |
| ----------------- | ------------------------------------------------------ |
| **Pinned commit** | `80fb31ebb7e8dfa5c26fc51ca81da8e06b618a52`             |
| **Pinned on**     | 2026-10-12                                             |
| **Local clone**   | `refs/opencode` (gitignored, created by `sync-ref.sh`) |

All notes are written against the pinned commit. After a re-pin, notes
pinned to an older SHA keep their own SHA in their header.

## Notes

| #   | Topic                                 | Commit    |
| --- | ------------------------------------- | --------- |
| 01  | [Paste summary](01-paste-summary.md)  | `03e6717` |
| 02  | [Providers and models](02-providers.md) | `03e6717` |
| 03  | [Context management & token use](03-context-management.md) | `03e6717` |
| 04  | [Images](04-images.md)                | `80fb31e` |
| 05  | [Memory](05-memory.md)                | `80fb31e` |

## Change reviews

What opencode changed between two pins, written by the `opencode-changes`
skill, which also moves the pin.

| Date | Range | Report |
| ---- | ----- | ------ |
| 2026-10-12 | `03e6717..80fb31e` (v2 promotion) | [2026-10-12](changes/2026-10-12.md) |

## Coverage checklist

- [ ] Architecture overview
- [ ] Agent loop
- [ ] Tool system (built-in tools)
- [x] LLM provider abstraction
- [ ] Sessions & persistence
- [ ] Client/server + TUI
- [ ] Permissions
- [x] Config (providers and model only)
- [ ] Agents/modes & prompts
- [x] Context management & compaction
- [ ] MCP
- [ ] LSP integration
- [ ] Plugins/extensibility
- [ ] Snapshots/undo
- [ ] CLI & non-interactive mode
