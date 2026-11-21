# nth-format

Notes for working in this crate. The root `CLAUDE.md` has the commands, the crate map and the rules every crate keeps.

the formatters run after a tool writes a file: opencode's built-in table (`registry.rs`), each with a probe for whether it applies to the project, plus custom ones from `[format]` in the config. `Formatters::format` runs every match; `nth formatters` prints `Formatters::status`.
