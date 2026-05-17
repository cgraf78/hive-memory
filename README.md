# hive-memory

[![CI](https://github.com/cgraf78/hive-memory/actions/workflows/ci.yml/badge.svg)](https://github.com/cgraf78/hive-memory/actions/workflows/ci.yml)
[![Release](https://github.com/cgraf78/hive-memory/actions/workflows/release.yml/badge.svg)](https://github.com/cgraf78/hive-memory/actions/workflows/release.yml)

Vendor-neutral shared memory infrastructure for AI agents.

Status: v1 implementation in progress. The normative v1 behavior is defined by
the spec; changes to command behavior, file formats, or hook contracts should
update `SPEC.md` in the same commit.

The primary binary is `hm`.

The current design plan lives in [PLAN.md](PLAN.md), and the normative v1 implementation spec lives in [SPEC.md](SPEC.md). The goal is to build a configurable, backend-agnostic memory layer that gives agent sessions across Linux, macOS, and WSL a shared hive-brain while keeping the canonical store human-readable, durable, and conflict-resistant.

## License

MIT. See [LICENSE](LICENSE).

## Contributing

Keep changes small and testable. Run `cargo fmt --check`, `cargo test`, and
`cargo clippy --all-targets --all-features -- -D warnings` before sending a
change. If a change affects public behavior, update `SPEC.md`; if it changes
the broader rationale, update `PLAN.md`.
