# <Topic>

|                     |                         |
| ------------------- | ----------------------- |
| **opencode commit** | `<full SHA>`            |
| **Researched**      | <YYYY-MM-DD>            |
| **Sources**         | source · docs · <other> |

## Summary

<3–5 lines: what this is and the one idea worth remembering.>

## User-facing behaviour

<What a user sees or configures. Mark each point `[source]` or `[docs]`.>

## Where it lives

```
packages/<pkg>/src/
├── <dir>/
│   └── <file>.ts    # <role>
└── <file>.ts        # <role>
```

## How it works

<Step-by-step code path from entry point to result. Each step cites
`path:line` + permalink. Add a mermaid diagram if the flow branches.>

1. **<Step>** — <what happens> ([`path:line`](https://github.com/anomalyco/opencode/blob/<SHA>/path#L<line>))

## Key data types / interfaces

<The few types that shape the design. Short excerpts only (≤ 15 lines).>

## Design decisions & trade-offs

- **Observed:** <choice seen in code> — <why it matters>
- **Inferred:** <reasoning not directly stated in code>

## Takeaways for nth

- **Copy:** <what to adopt, in Rust/Tokio terms>
- **Avoid:** <what not to repeat, and why>
- **Do differently:** <our alternative>

## Open questions

- <things not yet understood or worth a follow-up note>
