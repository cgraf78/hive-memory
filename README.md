# hive-memory

Vendor-neutral shared memory infrastructure for AI agents.

Status: design/planning phase. Implementation has not started yet.

The planned primary binary is `hm`.

The current design plan lives in [PLAN.md](PLAN.md), and the normative v1 implementation spec lives in [SPEC.md](SPEC.md). The goal is to build a configurable, backend-agnostic memory layer that gives agent sessions across Linux, macOS, and WSL a shared hive-brain while keeping the canonical store human-readable, durable, and conflict-resistant.

## License

MIT. See [LICENSE](LICENSE).
