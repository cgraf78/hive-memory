# Examples

These files demonstrate Hive Memory's caller-owned policy without containing
real memories, hostnames, repository remotes, or agent-specific integration.
The Rust test suite loads the exact checked-in files through the production
configuration and project-identity code.

## Configuration layers

Copy `config.toml` to the XDG configuration directory:

```text
${XDG_CONFIG_HOME:-$HOME/.config}/hive-memory/config.toml
```

It demonstrates two stores, safe retrieval defaults, explicit privacy policy,
offline writes, and opt-in classification. Replace the store paths and remove
the second store if one durable memory root is sufficient.

`config.local.toml` demonstrates recursive machine-local overrides. It replaces
only `host_id` and the personal store root; all other fields remain inherited
from `config.toml`. Copy it beside the main config only when a machine needs
those overrides, and do not commit a real host identity into a public repo.

Validate the layered files with:

```sh
hm --config /path/to/config.toml doctor --quick
```

## Stable project identity

Copy `.hive-memory-project` to a project root and replace `example-project`
with a stable, non-secret id. Hive Memory finds the marker while walking parent
directories, so commands may start from nested files or directories. Use the
same id in every checkout that should share project-scoped recall.
