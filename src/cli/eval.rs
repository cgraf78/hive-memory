//! Eval fixture CLI arguments, output models, and command handlers.

use anyhow::Result;
use clap::{Args, Subcommand};
use hive_memory::eval as memory_eval;
use serde::Serialize;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Eval fixture helper commands.
#[derive(Debug, Subcommand)]
pub(crate) enum EvalCommand {
    /// Run a retrieval corpus and report A/B metrics.
    Retrieval(EvalRetrievalArgs),
    /// Capture a recall miss as a retrieval eval case.
    CaptureMiss(EvalCaptureMissArgs),
    /// Capture an irrelevant retrieval hit as a retrieval eval case.
    CaptureBadHit(EvalCaptureBadHitArgs),
}

impl EvalCommand {
    /// Return whether this invocation requires structured error output.
    pub(crate) fn wants_json(&self) -> bool {
        match self {
            Self::CaptureMiss(args) => args.common.json,
            Self::CaptureBadHit(args) => args.common.json,
            Self::Retrieval(_) => false,
        }
    }
}

/// Arguments for `hm eval retrieval`.
#[derive(Debug, Args)]
pub(crate) struct EvalRetrievalArgs {
    /// TOML corpus file containing records and retrieval_case labels.
    #[arg(long)]
    corpus: PathBuf,
    /// Search limit used for each retrieval case.
    #[arg(long, default_value_t = 5)]
    limit: usize,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Shared arguments for `hm eval capture-*`.
#[derive(Debug, Args)]
struct EvalCaptureCommonArgs {
    /// Prompt or query that exposed the retrieval behavior.
    #[arg(long)]
    prompt: String,
    /// Optional human-readable case name. Defaults to a prompt-derived name.
    #[arg(long)]
    name: Option<String>,
    /// Feature bucket this case should score.
    #[arg(long, default_value = "semantic")]
    feature: String,
    /// Project id the query should run under, when project-scoped.
    #[arg(long)]
    project_id: Option<String>,
    /// Append the generated case to this TOML fixture file.
    #[arg(long)]
    to: Option<PathBuf>,
    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,
}

/// Arguments for `hm eval capture-miss`.
#[derive(Debug, Args)]
pub(crate) struct EvalCaptureMissArgs {
    #[command(flatten)]
    common: EvalCaptureCommonArgs,
    /// Subject id that should have been retrieved. Repeat for multiple labels.
    #[arg(long, required = true)]
    expected: Vec<String>,
    /// Subject id that must not be retrieved. Repeat for multiple labels.
    #[arg(long)]
    forbidden: Vec<String>,
}

/// Arguments for `hm eval capture-bad-hit`.
#[derive(Debug, Args)]
pub(crate) struct EvalCaptureBadHitArgs {
    #[command(flatten)]
    common: EvalCaptureCommonArgs,
    /// Subject id that was incorrectly retrieved. Repeat for multiple labels.
    #[arg(long, required = true)]
    bad: Vec<String>,
    /// Subject id that should be retrieved, if known. Repeat for multiple labels.
    #[arg(long)]
    expected: Vec<String>,
}

/// Run one eval command.
pub(crate) fn run(command: EvalCommand) -> Result<()> {
    match command {
        EvalCommand::Retrieval(args) => run_retrieval(args),
        EvalCommand::CaptureMiss(args) => run_capture_miss(args),
        EvalCommand::CaptureBadHit(args) => run_capture_bad_hit(args),
    }
}

fn run_retrieval(args: EvalRetrievalArgs) -> Result<()> {
    let report = memory_eval::run_retrieval_eval(memory_eval::RetrievalEvalInput {
        corpus_path: args.corpus,
        limit: args.limit,
    })?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_retrieval_report(&report, args.limit);
    }
    Ok(())
}

fn print_retrieval_report(report: &memory_eval::RetrievalEvalReport, limit: usize) {
    println!("corpus: {}", report.corpus);
    for candidate in &report.candidates {
        println!("candidate: {}", candidate.name);
        for metric in &candidate.features {
            println!(
                "  {} cases={} recall@{}={:.3} precision@{}={:.3} mrr={:.3} forbidden_hits={} p95_ms={}",
                metric.feature,
                metric.cases,
                limit,
                metric.recall_at_k,
                limit,
                metric.precision_at_k,
                metric.mrr,
                metric.forbidden_hits,
                metric.p95_ms
            );
        }
    }
}

fn run_capture_miss(args: EvalCaptureMissArgs) -> Result<()> {
    let snippet = render_retrieval_case(EvalRetrievalCaseInput {
        common: &args.common,
        expected: &args.expected,
        forbidden: &args.forbidden,
        note: "Captured from hm eval capture-miss; verify labels before relying on this case.",
    })?;
    emit_capture(&args.common, snippet)
}

fn run_capture_bad_hit(args: EvalCaptureBadHitArgs) -> Result<()> {
    let snippet = render_retrieval_case(EvalRetrievalCaseInput {
        common: &args.common,
        expected: &args.expected,
        forbidden: &args.bad,
        note: "Captured from hm eval capture-bad-hit; verify labels before relying on this case.",
    })?;
    emit_capture(&args.common, snippet)
}

struct EvalRetrievalCaseInput<'a> {
    common: &'a EvalCaptureCommonArgs,
    expected: &'a [String],
    forbidden: &'a [String],
    note: &'a str,
}

#[derive(Debug, Serialize)]
struct EvalCaptureOutput {
    snippet: String,
    path: Option<String>,
    appended: bool,
}

fn render_retrieval_case(input: EvalRetrievalCaseInput<'_>) -> Result<String> {
    if input.common.prompt.trim().is_empty() {
        anyhow::bail!("--prompt must not be empty");
    }
    if input.common.feature.trim().is_empty() {
        anyhow::bail!("--feature must not be empty");
    }

    let name = input
        .common
        .name
        .clone()
        .unwrap_or_else(|| captured_case_name(&input.common.prompt));
    let mut snippet = String::new();
    snippet.push_str("[[retrieval_case]]\n");
    snippet.push_str(&format!("name = {}\n", toml_string(&name)?));
    snippet.push_str(&format!(
        "feature = {}\n",
        toml_string(&input.common.feature)?
    ));
    snippet.push_str(&format!("query = {}\n", toml_string(&input.common.prompt)?));
    if let Some(project_id) = input.common.project_id.as_deref() {
        if project_id.trim().is_empty() {
            anyhow::bail!("--project-id must not be empty when provided");
        }
        snippet.push_str(&format!("project_id = {}\n", toml_string(project_id)?));
    }
    snippet.push_str(&format!(
        "expected = {}\n",
        toml_string_list(input.expected)?
    ));
    snippet.push_str(&format!(
        "forbidden = {}\n",
        toml_string_list(input.forbidden)?
    ));
    snippet.push_str("target_recall_at_5 = 1.0\n");
    snippet.push_str("target_precision_at_5 = 1.0\n");
    snippet.push_str(&format!("note = {}\n", toml_string(input.note)?));
    Ok(snippet)
}

fn emit_capture(common: &EvalCaptureCommonArgs, snippet: String) -> Result<()> {
    let mut appended = false;
    if let Some(path) = &common.to {
        append_snippet(path, &snippet)?;
        appended = true;
    }

    if common.json {
        let output = EvalCaptureOutput {
            snippet,
            path: common.to.as_ref().map(|path| path.display().to_string()),
            appended,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        print!("{snippet}");
        if !snippet.ends_with('\n') {
            println!();
        }
        if let Some(path) = &common.to {
            eprintln!("appended: {}", path.display());
        }
    }
    Ok(())
}

fn append_snippet(path: &Path, snippet: &str) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let needs_separator = file.metadata()?.len() > 0;
    if needs_separator {
        writeln!(file)?;
    }
    file.write_all(snippet.as_bytes())?;
    Ok(())
}

fn captured_case_name(prompt: &str) -> String {
    let mut words = prompt
        .split_whitespace()
        .filter_map(|word| {
            let normalized = word
                .trim_matches(|ch: char| !ch.is_ascii_alphanumeric())
                .to_ascii_lowercase();
            (!normalized.is_empty()).then_some(normalized)
        })
        .take(8)
        .collect::<Vec<_>>();
    if words.is_empty() {
        words.push("prompt".to_owned());
    }
    format!("captured {}", words.join(" "))
}

fn toml_string(value: &str) -> Result<String> {
    Ok(serde_json::to_string(value)?)
}

fn toml_string_list(values: &[String]) -> Result<String> {
    let rendered = values
        .iter()
        .map(|value| toml_string(value))
        .collect::<Result<Vec<_>>>()?;
    Ok(format!("[{}]", rendered.join(", ")))
}
