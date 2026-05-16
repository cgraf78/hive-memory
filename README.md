# hive-memory

Vendor-neutral shared memory infrastructure for AI agents.

The planned primary binary is `hm`.

The current design plan lives in [PLAN.md](PLAN.md). The goal is to build a configurable, backend-agnostic memory layer that gives agent sessions across Linux, macOS, and WSL a shared hive-brain while keeping the canonical store human-readable, durable, and conflict-resistant.
