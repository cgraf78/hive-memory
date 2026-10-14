//! `hm get`: print one memory by id.

use crate::{
    CliContext, StoreAccess, load_config, project_binding_store, rebuild_store_index,
    resolve_agent_id, resolve_project_id, resolve_store,
};
use anyhow::{Context as _, Result, anyhow};
use clap::Args;
use hive_memory::config::Config;
use hive_memory::{curated, note, visibility};
use serde::Serialize;
use std::io::Write as _;

/// Arguments for `hm get`.
#[derive(Debug, Args)]
pub(crate) struct GetArgs {
    /// Memory id, as shown in context blocks and search results (curated
    /// files use `curated:<path>`).
    id: String,
    /// Active project path or file hint, for project store bindings.
    /// Defaults to `HIVE_MEMORY_PROJECT`.
    #[arg(long)]
    project: Option<String>,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

impl GetArgs {
    /// Return whether this invocation requires structured error output.
    pub(crate) fn wants_json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Serialize)]
struct GetOutput {
    id: String,
    store: String,
    scope: String,
    project_id: Option<String>,
    note_path: String,
    body: String,
}

/// Print one memory, including ones too large for hook context.
///
/// Hook context points here for oversized memories, so the store is resolved
/// the way hook context resolves it (explicit `--store`, then the project's
/// store binding, then the agent default), and without an explicit `--store`
/// every other store the agent may read is searched too. Notes come from the
/// local index projection first, because the canonical store can be a slow or
/// offline cloud mount; curated files are read from the store. The store's
/// read policy and the memory's audience always apply.
pub(crate) fn run(args: GetArgs, context: CliContext) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    let agent_id = resolve_agent_id(context.as_agent.clone());
    let project_id = resolve_project_id(None, args.project.as_deref())?;
    let binding = project_binding_store(&config, project_id.as_deref())?;
    let primary = resolve_store(
        &config,
        context.store.as_deref(),
        binding.as_deref(),
        agent_id.as_deref(),
        StoreAccess::Read,
    )?
    .name;
    let mut stores = vec![primary.clone()];
    if context.store.is_none() {
        let mut others = config
            .stores
            .keys()
            .filter(|name| **name != primary)
            .filter(|name| {
                resolve_store(
                    &config,
                    Some(name.as_str()),
                    None,
                    agent_id.as_deref(),
                    StoreAccess::Read,
                )
                .is_ok()
            })
            .cloned()
            .collect::<Vec<_>>();
        others.sort();
        stores.extend(others);
    }

    let mut found = None;
    for (index, store) in stores.iter().enumerate() {
        found = match find_memory(&config, store, &args.id, agent_id.as_deref()) {
            Ok(found) => found,
            // An unreachable fallback store must not hide the memory in a
            // store searched after it.
            Err(err) if index > 0 => {
                eprintln!("warning: skipped store {store}: {err:#}");
                None
            }
            Err(err) => return Err(err),
        };
        if found.is_some() {
            break;
        }
    }
    let output = found.ok_or_else(|| {
        anyhow!(
            "memory {} not found in readable stores: {}",
            args.id,
            stores.join(", ")
        )
    })?;

    let rendered = if args.json {
        format!("{}\n", serde_json::to_string_pretty(&output)?)
    } else {
        let mut text = format!(
            "id: {}\nstore: {}\nscope: {}\n",
            output.id, output.store, output.scope
        );
        if let Some(project_id) = &output.project_id {
            text.push_str(&format!("project_id: {project_id}\n"));
        }
        text.push('\n');
        text.push_str(&output.body);
        if !output.body.ends_with('\n') {
            text.push('\n');
        }
        text
    };
    // Agents often pipe this into `head`; a closed pipe is not an error.
    match std::io::stdout().lock().write_all(rendered.as_bytes()) {
        Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        result => result.context("write output"),
    }
}

/// Look up one memory id in one store, or `None` when it is absent or hidden.
fn find_memory(
    config: &Config,
    store: &str,
    id: &str,
    agent_id: Option<&str>,
) -> Result<Option<GetOutput>> {
    let root = &config.stores[store].root;
    if id.starts_with("curated:") {
        let file = curated::collect_all(root)?
            .into_iter()
            .find(|file| file.id == id);
        return Ok(file.map(|file| GetOutput {
            id: file.id,
            store: store.to_owned(),
            scope: file.scope,
            project_id: file.project_id,
            note_path: file.relative_path,
            body: file.body,
        }));
    }
    let entries = match super::hook::load_cached_store_index(config, store)? {
        Some(report) => report.entries,
        None => rebuild_store_index(config, store)?.entries,
    };
    let Some(entry) = entries
        .into_iter()
        .find(|entry| entry.id == id && visibility::audience_allows(entry, agent_id))
    else {
        return Ok(None);
    };
    let body = if entry.body.is_empty() {
        // Entries from an older index schema carry no body.
        let path = root.join(&entry.note_path);
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("read note {}", path.display()))?;
        note::parse_note(&contents)
            .with_context(|| format!("parse note {}", path.display()))?
            .body
    } else {
        entry.body
    };
    Ok(Some(GetOutput {
        id: entry.id,
        store: store.to_owned(),
        scope: entry.scope,
        project_id: entry.project_id,
        note_path: entry.note_path,
        body,
    }))
}
