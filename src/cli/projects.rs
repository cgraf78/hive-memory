//! Project identity CLI arguments, output models, and command handlers.

use crate::{
    CliContext, NoIdentityPolicy, StoreAccess, hook_options, load_config, read_store_manifest,
    resolve_agent_id, resolve_store, resolve_store_with_policy,
};
use anyhow::Result;
use clap::{Args, Subcommand};
use hive_memory::project;
use serde::Serialize;
use std::path::PathBuf;

/// Project identity commands.
#[derive(Debug, Subcommand)]
pub(crate) enum ProjectsCommand {
    /// List local project-to-store bindings.
    List(ProjectListArgs),
    /// Show local policy and aliases for one project.
    Show(ProjectShowArgs),
    /// Resolve a path/file hint to a stable project id.
    Resolve(ProjectResolveArgs),
    /// Bind a project to a local preferred store.
    Bind(ProjectBindArgs),
    /// Remove a local project store binding.
    Unbind(ProjectUnbindArgs),
    /// Record that an old project id now maps to a new project id.
    Alias(ProjectAliasArgs),
}

impl ProjectsCommand {
    /// Return whether this invocation requires structured error output.
    pub(crate) fn wants_json(&self) -> bool {
        match self {
            Self::Resolve(args) => args.json,
            Self::Bind(args) => args.json,
            Self::Unbind(args) => args.json,
            Self::List(_) | Self::Show(_) | Self::Alias(_) => false,
        }
    }
}

/// Arguments for `hm projects resolve`.
#[derive(Debug, Args)]
pub(crate) struct ProjectResolveArgs {
    /// Path, file, or directory hint as an option, matching other agent-facing commands.
    #[arg(long, value_name = "PATH", conflicts_with = "path")]
    project: Option<PathBuf>,
    /// Path, file, or directory hint. Defaults to HIVE_MEMORY_PROJECT, then CWD.
    #[arg(value_name = "PATH")]
    path: Option<PathBuf>,
    /// Explicit project id override.
    #[arg(long)]
    project_id: Option<String>,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm projects list`.
#[derive(Debug, Args)]
pub(crate) struct ProjectListArgs {
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm projects show`.
#[derive(Debug, Args)]
pub(crate) struct ProjectShowArgs {
    /// Project id to inspect. Defaults to resolving the current project hint.
    project_id: Option<String>,
    /// Path, file, or directory hint used when no project id is provided.
    #[arg(long)]
    path: Option<PathBuf>,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm projects bind`.
#[derive(Debug, Args)]
pub(crate) struct ProjectBindArgs {
    /// Path, file, or directory hint for the project to bind.
    path: PathBuf,
    /// Store alias to prefer for this project on this machine.
    #[arg(long)]
    store: String,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm projects unbind`.
#[derive(Debug, Args)]
pub(crate) struct ProjectUnbindArgs {
    /// Path, file, or directory hint for the project to unbind.
    path: PathBuf,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm projects alias`.
#[derive(Debug, Args)]
pub(crate) struct ProjectAliasArgs {
    /// Prior project id to preserve.
    old_id: String,
    /// Canonical/current project id.
    new_id: String,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Serialize)]
struct ProjectBindOutput {
    project_id: String,
    store: String,
    binding: String,
}

#[derive(Debug, Serialize)]
struct ProjectUnbindOutput {
    project_id: String,
    removed: bool,
    binding: Option<String>,
}

#[derive(Debug, Serialize)]
struct ProjectResolveOutput {
    project_id: String,
    project_root: String,
    project_hint: String,
    project_source: String,
    store: String,
    store_source: String,
}

#[derive(Debug, Serialize)]
struct ProjectAliasOutput {
    old_id: String,
    new_id: String,
    store: String,
    store_source: String,
    path: String,
}

#[derive(Debug, Serialize)]
struct ProjectListOutput {
    data_dir: String,
    bindings: Vec<project::ProjectBinding>,
}

#[derive(Debug, Serialize)]
struct ProjectShowOutput {
    project_id: String,
    binding: Option<project::ProjectBinding>,
    effective_store: String,
    store_source: String,
    related_project_ids: Vec<String>,
}

/// Run one project identity command.
pub(crate) fn run(command: ProjectsCommand, context: CliContext) -> Result<()> {
    match command {
        ProjectsCommand::List(args) => run_list(args, context),
        ProjectsCommand::Show(args) => run_show(args, context),
        ProjectsCommand::Resolve(args) => run_resolve(args, context),
        ProjectsCommand::Bind(args) => run_bind(args, context),
        ProjectsCommand::Unbind(args) => run_unbind(args, context),
        ProjectsCommand::Alias(args) => run_alias(args, context),
    }
}

fn run_list(args: ProjectListArgs, context: CliContext) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    let bindings = project::list_bindings(&config.data_dir)?;

    if args.json {
        let output = ProjectListOutput {
            data_dir: config.data_dir.display().to_string(),
            bindings,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("data_dir: {}", config.data_dir.display());
        println!("bindings: {}", bindings.len());
        for binding in bindings {
            println!("  {} -> {}", binding.project_id, binding.store);
        }
    }

    Ok(())
}

fn run_show(args: ProjectShowArgs, context: CliContext) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    let project_id = if let Some(project_id) = args.project_id {
        project_id
    } else {
        let hint = args
            .path
            .or_else(|| std::env::var("HIVE_MEMORY_PROJECT").ok().map(PathBuf::from))
            .unwrap_or_default();
        project::resolve_project(project::ResolveProjectInput {
            hint,
            explicit_project_id: None,
            env_project_id: std::env::var("HIVE_MEMORY_PROJECT_ID").ok(),
        })?
        .project_id
    };
    let binding = project::load_binding(&config.data_dir, &project_id)?;
    let agent_id = resolve_agent_id(context.as_agent);
    // Humans may inspect any configured store, including a non-default bound
    // store; asserted agents remain subject to their read policy.
    let store = resolve_store_with_policy(
        &config,
        context.store.as_deref(),
        binding.as_ref().map(|binding| binding.store.as_str()),
        agent_id.as_deref(),
        StoreAccess::Read,
        NoIdentityPolicy::AllowAnyStore,
    )?;
    let store_config = &config.stores[store.name.as_str()];
    let aliases = project::related_project_ids(&store_config.root, &project_id)?
        .into_iter()
        .filter(|related| related != &project_id)
        .collect::<Vec<_>>();

    if args.json {
        let output = ProjectShowOutput {
            project_id,
            binding,
            effective_store: store.name,
            store_source: store.source.to_string(),
            related_project_ids: aliases,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("project_id: {}", project_id);
        match &binding {
            Some(binding) => println!("binding: {}", binding.store),
            None => println!("binding: none"),
        }
        println!("store: {}", store.name);
        println!("store_source: {}", store.source);
        if aliases.is_empty() {
            println!("related_project_ids: none");
        } else {
            println!("related_project_ids:");
            for alias in aliases {
                println!("  {alias}");
            }
        }
    }

    Ok(())
}

fn run_resolve(args: ProjectResolveArgs, context: CliContext) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    let hint = args
        .project
        .or(args.path)
        .or_else(|| std::env::var("HIVE_MEMORY_PROJECT").ok().map(PathBuf::from))
        .unwrap_or_default();
    let project = project::resolve_project(project::ResolveProjectInput {
        hint,
        explicit_project_id: args.project_id,
        env_project_id: std::env::var("HIVE_MEMORY_PROJECT_ID").ok(),
    })?;
    let agent_id = resolve_agent_id(context.as_agent);
    let binding = project::load_binding(&config.data_dir, &project.project_id)?;
    // Humans may resolve any configured store, including a non-default bound
    // store; asserted agents remain subject to their read policy.
    let store = resolve_store_with_policy(
        &config,
        context.store.as_deref(),
        binding.as_ref().map(|binding| binding.store.as_str()),
        agent_id.as_deref(),
        StoreAccess::Read,
        NoIdentityPolicy::AllowAnyStore,
    )?;

    if args.json {
        let output = ProjectResolveOutput {
            project_id: project.project_id,
            project_root: project.project_root.display().to_string(),
            project_hint: project.project_hint.display().to_string(),
            project_source: project.source.to_string(),
            store: store.name,
            store_source: store.source.to_string(),
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("project_id: {}", project.project_id);
        println!("project_root: {}", project.project_root.display());
        println!("project_hint: {}", project.project_hint.display());
        println!("project_source: {}", project.source);
        println!("store: {}", store.name);
        println!("store_source: {}", store.source);
    }

    Ok(())
}

fn run_bind(args: ProjectBindArgs, context: CliContext) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    let project = project::resolve_project(project::ResolveProjectInput {
        hint: args.path,
        explicit_project_id: None,
        env_project_id: std::env::var("HIVE_MEMORY_PROJECT_ID").ok(),
    })?;
    let agent_id = resolve_agent_id(context.as_agent);
    // Bindings affect both read and write commands, so validate both sides for
    // asserted agents. Humans may record any configured local affinity.
    resolve_store_with_policy(
        &config,
        Some(args.store.as_str()),
        None,
        agent_id.as_deref(),
        StoreAccess::Read,
        NoIdentityPolicy::AllowAnyStore,
    )?;
    resolve_store_with_policy(
        &config,
        Some(args.store.as_str()),
        None,
        agent_id.as_deref(),
        StoreAccess::Write,
        NoIdentityPolicy::AllowAnyStore,
    )?;
    let binding = project::ProjectBinding {
        project_id: project.project_id.clone(),
        store: args.store,
    };
    let path = project::save_binding(&config.data_dir, &binding, &hook_options(&config))?;

    if args.json {
        let output = ProjectBindOutput {
            project_id: project.project_id,
            store: binding.store,
            binding: path.display().to_string(),
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("project_id: {}", project.project_id);
        println!("store: {}", binding.store);
        println!("binding: {}", path.display());
    }
    Ok(())
}

fn run_unbind(args: ProjectUnbindArgs, context: CliContext) -> Result<()> {
    let config = load_config(context.config_path.as_deref())?;
    let project = project::resolve_project(project::ResolveProjectInput {
        hint: args.path,
        explicit_project_id: None,
        env_project_id: std::env::var("HIVE_MEMORY_PROJECT_ID").ok(),
    })?;
    let removed = project::remove_binding(&config.data_dir, &project.project_id)?;

    if args.json {
        let output = ProjectUnbindOutput {
            project_id: project.project_id,
            removed: removed.is_some(),
            binding: removed.map(|path| path.display().to_string()),
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("project_id: {}", project.project_id);
        println!("removed: {}", removed.is_some());
        if let Some(path) = removed {
            println!("binding: {}", path.display());
        }
    }
    Ok(())
}

fn run_alias(args: ProjectAliasArgs, context: CliContext) -> Result<()> {
    if args.old_id == args.new_id {
        anyhow::bail!("old and new project ids must differ");
    }
    let config = load_config(context.config_path.as_deref())?;
    let agent_id = resolve_agent_id(context.as_agent);
    let store = resolve_store(
        &config,
        context.store.as_deref(),
        None,
        agent_id.as_deref(),
        StoreAccess::Write,
    )?;
    let store_config = &config.stores[store.name.as_str()];
    read_store_manifest(&config, &store.name, store_config)?;
    let path = project::add_alias(
        &store_config.root,
        &args.old_id,
        &args.new_id,
        &hook_options(&config),
    )?;

    if args.json {
        let output = ProjectAliasOutput {
            old_id: args.old_id,
            new_id: args.new_id,
            store: store.name,
            store_source: store.source.to_string(),
            path: path.display().to_string(),
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("old_id: {}", args.old_id);
        println!("new_id: {}", args.new_id);
        println!("store: {}", store.name);
        println!("store_source: {}", store.source);
        println!("aliases: {}", path.display());
    }

    Ok(())
}
