//! Black-box security-boundary tests for hive-memory (`hm`).
//!
//! These exercise verified trust/visibility boundaries through the real CLI so
//! a regression in store policy, curated discovery, or audience filtering shows
//! up as a failing command rather than a silent privacy leak:
//!
//! - project-alias ids cannot escape `memories/projects/` to inject arbitrary
//!   `.md` at the highest (`curated`) trust level,
//! - the per-agent store allowlist fails closed when no identity is asserted,
//!   without breaking the plain human default-store path,
//! - `agent-private` audience filtering is enforced end to end, and
//! - curated discovery surfaces the expected files while never following a
//!   symlink out of the store.

use assert_cmd::cargo::cargo_bin_cmd;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "hive-memory-sec-{name}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("create temp dir");
    path
}

/// Initialize a store root with a manifest via the real `hm stores init`.
fn init_store(root: &Path, name: &str) {
    cargo_bin_cmd!("hm")
        .args([
            "stores",
            "init",
            name,
            "--root",
            root.to_str().expect("utf8 path"),
        ])
        .assert()
        .success();
}

/// Single-store config with `personal` as the default store.
fn write_single_store_config(config: &Path, dir: &Path, personal_root: &Path) {
    fs::write(
        config,
        format!(
            r#"
            default_store = "personal"
            data_dir = "{}"
            state_dir = "{}"
            cache_dir = "{}"

            [stores.personal]
            root = "{}"
            "#,
            dir.join("data").display(),
            dir.join("state").display(),
            dir.join("cache").display(),
            personal_root.display(),
        ),
    )
    .expect("write config");
}

/// The active project id used by alias-escape tests. A flat slug like a real id.
const ACTIVE_PROJECT_ID: &str = "github-com-cgraf78-hive-memory-018f5f57";

// ---------------------------------------------------------------------------
// 1. (HIGH) project-alias path escape + curated-trust injection
// ---------------------------------------------------------------------------

#[test]
fn alias_path_escape_is_not_injected_into_context() {
    let dir = temp_dir("alias-escape");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    write_single_store_config(&config, &dir, &personal);
    init_store(&personal, "personal");

    // The active project's curated directory holds a legitimate memory plus a
    // hostile aliases.toml that points outside the store, mimicking a synced or
    // tampered store.
    let active_dir = personal.join("memories/projects").join(ACTIVE_PROJECT_ID);
    fs::create_dir_all(&active_dir).expect("active project dir");
    fs::write(
        active_dir.join("memory.md"),
        "Legitimate project memory for the active project.\n",
    )
    .expect("write legit memory");

    // A sentinel file OUTSIDE the store that the relative-escape alias would
    // reach if the join were not sanitized: ../../../../<dir>/evil-escape.
    let sentinel_dir = dir.join("evil-escape");
    fs::create_dir_all(&sentinel_dir).expect("sentinel dir");
    let sentinel_secret = "SENTINEL-CURATED-INJECTION-SHOULD-NOT-APPEAR";
    fs::write(sentinel_dir.join("evil.md"), format!("{sentinel_secret}\n"))
        .expect("write sentinel");

    // Compute a relative escape from memories/projects/<id>/ back up to the
    // sentinel directory: memories(1)/projects(2)/<id>(3) -> 3 hops to store
    // root, +1 to reach `dir`, then into evil-escape.
    let relative_escape = "../../../../evil-escape";
    let absolute_escape = sentinel_dir.to_str().expect("utf8 sentinel");

    fs::write(
        active_dir.join("aliases.toml"),
        format!(
            "schema_version = 1\nproject_id = \"{ACTIVE_PROJECT_ID}\"\naliases = [\"{relative_escape}\", \"{absolute_escape}\"]\n",
        ),
    )
    .expect("write hostile aliases");

    let output = cargo_bin_cmd!("hm")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "context",
            "--project-id",
            ACTIVE_PROJECT_ID,
        ])
        .output()
        .expect("run context");
    assert!(output.status.success(), "context failed: {output:?}");
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");

    // The legitimate project memory still resolves through its curated dir.
    assert!(
        stdout.contains("Legitimate project memory for the active project."),
        "expected legit project memory in context: {stdout}"
    );
    // The out-of-store sentinel must never be injected, at any trust level.
    assert!(
        !stdout.contains(sentinel_secret),
        "path-escaping alias leaked an outside file into context: {stdout}"
    );
}

#[test]
fn normal_alias_id_still_resolves_curated_dir() {
    let dir = temp_dir("alias-normal");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    write_single_store_config(&config, &dir, &personal);
    init_store(&personal, "personal");

    // A renamed project: memory lives under the OLD id, and the current id's
    // aliases.toml lists the old id as a normal slug. Context for the current
    // id must follow the alias chain and surface the old-id memory.
    let old_id = "old-project-slug";
    let old_dir = personal.join("memories/projects").join(old_id);
    fs::create_dir_all(&old_dir).expect("old project dir");
    fs::write(
        old_dir.join("memory.md"),
        "Memory stored under the pre-rename project id.\n",
    )
    .expect("write old memory");

    let current_dir = personal.join("memories/projects").join(ACTIVE_PROJECT_ID);
    fs::create_dir_all(&current_dir).expect("current project dir");
    fs::write(
        current_dir.join("aliases.toml"),
        format!(
            "schema_version = 1\nproject_id = \"{ACTIVE_PROJECT_ID}\"\naliases = [\"{old_id}\"]\n",
        ),
    )
    .expect("write aliases");

    let output = cargo_bin_cmd!("hm")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "context",
            "--project-id",
            ACTIVE_PROJECT_ID,
        ])
        .output()
        .expect("run context");
    assert!(output.status.success(), "context failed: {output:?}");
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    assert!(
        stdout.contains("Memory stored under the pre-rename project id."),
        "normal alias id failed to resolve curated dir: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// 3. (Tier-2-c) Allowlist fail-closed for missing identity
// ---------------------------------------------------------------------------

/// Config with two stores where the (unnamed) default policy must keep working
/// for humans against `personal`, while `work` is reachable only by an agent
/// that lists it.
fn write_restricted_config(config: &Path, dir: &Path, personal_root: &Path, work_root: &Path) {
    fs::write(
        config,
        format!(
            r#"
            default_store = "personal"
            data_dir = "{}"
            state_dir = "{}"
            cache_dir = "{}"

            [stores.personal]
            root = "{}"

            [stores.work]
            root = "{}"

            [agents.codex]
            default_store = "work"
            read_stores = ["work"]
            write_stores = ["work"]
            "#,
            dir.join("data").display(),
            dir.join("state").display(),
            dir.join("cache").display(),
            personal_root.display(),
            work_root.display(),
        ),
    )
    .expect("write config");
}

#[test]
fn no_identity_write_to_non_default_store_is_refused() {
    let dir = temp_dir("allowlist-refuse");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let work = dir.join("work");
    write_restricted_config(&config, &dir, &personal, &work);
    init_store(&personal, "personal");
    init_store(&work, "work");

    // No --as-agent: previously this skipped policy entirely and let --store
    // target ANY store. It must now be refused for a non-default store with
    // privacy-refusal exit code 4.
    let output = cargo_bin_cmd!("hm")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "--store",
            "work",
            "remember",
            "--text",
            "This must not be written to the non-default store without identity.",
        ])
        .output()
        .expect("run remember");
    assert_eq!(
        output.status.code(),
        Some(4),
        "expected privacy refusal (exit 4): {output:?}"
    );
}

#[test]
fn no_identity_write_to_default_store_succeeds() {
    let dir = temp_dir("allowlist-human-default");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let work = dir.join("work");
    write_restricted_config(&config, &dir, &personal, &work);
    init_store(&personal, "personal");
    init_store(&work, "work");

    // A plain human shell with NO --as-agent must keep working against the
    // default store. This is the path the fail-closed change must not break.
    cargo_bin_cmd!("hm")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "remember",
            "--text",
            "Human default-store write should still succeed without identity.",
        ])
        .assert()
        .success();
}

// ---------------------------------------------------------------------------
// (Tier-3) agent-private audience filtering through the real CLI
// ---------------------------------------------------------------------------

#[test]
fn agent_private_audience_is_filtered_by_invoking_agent() {
    let dir = temp_dir("agent-private");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    write_single_store_config(&config, &dir, &personal);
    init_store(&personal, "personal");

    // Body carries a distinctive token so `hm search <token>` matches on body
    // text (search filters on body/metadata, not the scope field).
    let search_token = "claudeonlytoken";
    let secret_body = "Agent-private memory only claude may read: claudeonlytoken.";
    cargo_bin_cmd!("hm")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "--as-agent",
            "claude",
            "remember",
            "--scope",
            "agent-private",
            "--audience",
            "claude",
            "--text",
            secret_body,
        ])
        .assert()
        .success();

    // codex must NOT see a claude-only record in search or context.
    let codex_search = cargo_bin_cmd!("hm")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "--as-agent",
            "codex",
            "search",
            "--scope",
            "agent-private",
            search_token,
        ])
        .output()
        .expect("run codex search");
    assert!(
        codex_search.status.success(),
        "search failed: {codex_search:?}"
    );
    let codex_search_out = String::from_utf8(codex_search.stdout).expect("utf8");
    assert!(
        !codex_search_out.contains(secret_body),
        "codex saw a claude-only agent-private record in search: {codex_search_out}"
    );

    let codex_context = cargo_bin_cmd!("hm")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "--as-agent",
            "codex",
            "context",
            "--scope",
            "agent-private",
        ])
        .output()
        .expect("run codex context");
    assert!(
        codex_context.status.success(),
        "context failed: {codex_context:?}"
    );
    let codex_context_out = String::from_utf8(codex_context.stdout).expect("utf8");
    assert!(
        !codex_context_out.contains(secret_body),
        "codex saw a claude-only agent-private record in context: {codex_context_out}"
    );

    // claude (the listed audience) must see it.
    let claude_search = cargo_bin_cmd!("hm")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "--as-agent",
            "claude",
            "search",
            "--scope",
            "agent-private",
            search_token,
        ])
        .output()
        .expect("run claude search");
    assert!(
        claude_search.status.success(),
        "search failed: {claude_search:?}"
    );
    let claude_search_out = String::from_utf8(claude_search.stdout).expect("utf8");
    assert!(
        claude_search_out.contains(secret_body),
        "claude could not see its own agent-private record: {claude_search_out}"
    );
}

#[test]
fn retag_cannot_read_or_declassify_agent_private_memory() {
    let dir = temp_dir("agent-private-retag");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    write_single_store_config(&config, &dir, &personal);
    init_store(&personal, "personal");

    let remembered = cargo_bin_cmd!("hm")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "--as-agent",
            "claude",
            "remember",
            "--scope",
            "agent-private",
            "--audience",
            "claude",
            "--text",
            "Only claude may retag this memory.",
            "--json",
        ])
        .output()
        .expect("write private memory");
    assert!(
        remembered.status.success(),
        "remember failed: {remembered:?}"
    );
    let remembered: serde_json::Value =
        serde_json::from_slice(&remembered.stdout).expect("remember json");
    let id = remembered["id"].as_str().expect("memory id");

    cargo_bin_cmd!("hm")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "--as-agent",
            "codex",
            "retag",
            id,
            "--kind",
            "reference",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("not visible to the active agent"));

    cargo_bin_cmd!("hm")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "--as-agent",
            "claude",
            "retag",
            id,
            "--scope",
            "global",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "cannot change agent-private visibility",
        ));
}

#[test]
fn retag_requires_read_and_write_store_access() {
    let dir = temp_dir("retag-store-access");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    fs::write(
        &config,
        format!(
            r#"
            default_store = "personal"
            data_dir = "{}"
            state_dir = "{}"
            cache_dir = "{}"

            [stores.personal]
            root = "{}"

            [agents.writer]
            default_store = "personal"
            read_stores = []
            write_stores = ["personal"]
            "#,
            dir.join("data").display(),
            dir.join("state").display(),
            dir.join("cache").display(),
            personal.display(),
        ),
    )
    .expect("write config");
    init_store(&personal, "personal");

    let remembered = cargo_bin_cmd!("hm")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "remember",
            "--text",
            "A write-only agent must not retag existing memory.",
            "--json",
        ])
        .output()
        .expect("write memory");
    assert!(
        remembered.status.success(),
        "remember failed: {remembered:?}"
    );
    let remembered: serde_json::Value =
        serde_json::from_slice(&remembered.stdout).expect("remember json");
    let id = remembered["id"].as_str().expect("memory id");

    let retag = cargo_bin_cmd!("hm")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "--as-agent",
            "writer",
            "retag",
            id,
            "--kind",
            "reference",
        ])
        .output()
        .expect("run retag");
    assert_eq!(
        retag.status.code(),
        Some(4),
        "expected privacy refusal: {retag:?}"
    );
    assert!(
        String::from_utf8(retag.stderr)
            .expect("utf8 stderr")
            .contains("may not read store personal")
    );
}

// ---------------------------------------------------------------------------
// (Tier-3) curated `collect`: surfaces curated files; never follows symlinks
// ---------------------------------------------------------------------------

#[test]
fn curated_collect_surfaces_files_and_skips_outside_symlink() {
    let dir = temp_dir("curated-collect");
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    write_single_store_config(&config, &dir, &personal);
    init_store(&personal, "personal");

    fs::create_dir_all(personal.join("people")).expect("people dir");
    fs::write(personal.join("people/p.md"), "Curated person note for p.\n")
        .expect("write people note");

    fs::create_dir_all(personal.join("memories/global")).expect("global dir");
    fs::write(
        personal.join("memories/global/g.md"),
        "Curated global memory g.\n",
    )
    .expect("write global note");

    let project_dir = personal.join("memories/projects").join(ACTIVE_PROJECT_ID);
    fs::create_dir_all(&project_dir).expect("project dir");
    fs::write(project_dir.join("m.md"), "Curated project memory m.\n").expect("write project note");
    // An alias so the project dir is reached even when the active id differs.
    fs::write(
        project_dir.join("aliases.toml"),
        format!(
            "schema_version = 1\nproject_id = \"{ACTIVE_PROJECT_ID}\"\naliases = [\"renamed-old\"]\n",
        ),
    )
    .expect("write aliases");

    // A sentinel outside the store, reachable only by following a symlink.
    let outside = dir.join("outside");
    fs::create_dir_all(&outside).expect("outside dir");
    let symlink_secret = "SYMLINK-OUTSIDE-CONTENT-MUST-NOT-BE-COLLECTED";
    fs::write(outside.join("leak.md"), format!("{symlink_secret}\n")).expect("write outside note");

    // A symlink under rules/ pointing at the outside file. Curated discovery
    // inspects entries without following symlinks, so this must be ignored.
    fs::create_dir_all(personal.join("rules")).expect("rules dir");
    symlink_file(&outside.join("leak.md"), &personal.join("rules/leak.md"));

    let output = cargo_bin_cmd!("hm")
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "context",
            "--project-id",
            ACTIVE_PROJECT_ID,
        ])
        .output()
        .expect("run context");
    assert!(output.status.success(), "context failed: {output:?}");
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");

    assert!(
        stdout.contains("Curated person note for p."),
        "missing people note: {stdout}"
    );
    assert!(
        stdout.contains("Curated global memory g."),
        "missing global note: {stdout}"
    );
    assert!(
        stdout.contains("Curated project memory m."),
        "missing project note: {stdout}"
    );
    assert!(
        !stdout.contains(symlink_secret),
        "curated discovery followed a symlink out of the store: {stdout}"
    );
}

#[cfg(unix)]
fn symlink_file(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).expect("create symlink");
}

#[cfg(not(unix))]
fn symlink_file(target: &Path, link: &Path) {
    std::os::windows::fs::symlink_file(target, link).expect("create symlink");
}
