//! End-to-end coverage for delivery-tracked lifecycle hooks.
//!
//! These spawn the real `hm hook` commands with the optional delivery flags
//! (`--delivery`, `--max-context-bytes`, `--reset`, `--context-size`) and the
//! `context-reset` event, and assert the JSON contract plus session state.
//! The invariant under test: a memory is skipped only after it was emitted
//! within the host limit in the current epoch, and tracked hooks never show
//! less than untracked ones. `hm get` is covered too, since oversized-memory
//! pointers name it.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

mod common;
use common::temp_dir;

macro_rules! cargo_bin_cmd {
    ("hm") => {
        common::hermetic_hm()
    };
}

const GLOBAL_A: &str = "Global delivery fact alpha keeps hook output observable.";
const GLOBAL_B: &str = "Global delivery fact bravo keeps hook output observable.";
const PROJECT_B: &str = "Project bravo uses the zephyrine release checklist.";

struct Fixture {
    config: PathBuf,
    state: PathBuf,
    repo_a: PathBuf,
    repo_b: PathBuf,
}

impl Fixture {
    /// Store with two global memories and one memory scoped to repo B.
    fn new(name: &str) -> Self {
        Self::with_strategy(name, "recency")
    }

    /// [`Fixture::new`] with another context strategy. Under `relevance`,
    /// reference memories are search-only: only prompt recall shows them.
    fn with_strategy(name: &str, strategy: &str) -> Self {
        let dir = temp_dir(name);
        let config = dir.join("config.toml");
        let personal = dir.join("personal");
        let state = dir.join("state");
        let repo_a = dir.join("repo-a");
        let repo_b = dir.join("repo-b");
        for (repo, id) in [(&repo_a, "delivery-repo-a"), (&repo_b, "delivery-repo-b")] {
            fs::create_dir_all(repo.join("src")).expect("repo");
            fs::write(
                repo.join(".hive-memory-project"),
                format!("id = \"{id}\"\n"),
            )
            .expect("marker");
        }
        fs::write(
            &config,
            format!(
                r#"
                default_store = "personal"
                state_dir = "{}"
                cache_dir = "{}"

                [stores.personal]
                root = "{}"

                [defaults]
                context_strategy = "{}"
                "#,
                state.display(),
                state.join("cache").display(),
                personal.display(),
                strategy
            ),
        )
        .expect("write config");
        cargo_bin_cmd!("hm")
            .args([
                "stores",
                "init",
                "personal",
                "--root",
                personal.to_str().expect("utf8"),
            ])
            .assert()
            .success();
        let fixture = Self {
            config,
            state,
            repo_a,
            repo_b,
        };
        fixture.remember(GLOBAL_A, None);
        fixture.remember(GLOBAL_B, None);
        fixture.remember(PROJECT_B, Some(&fixture.repo_b.clone()));
        fixture.refresh();
        fixture
    }

    fn remember(&self, text: &str, project: Option<&Path>) {
        let mut cmd = cargo_bin_cmd!("hm");
        cmd.args([
            "--config",
            self.config_str(),
            "--as-agent",
            "claude",
            "remember",
        ]);
        if let Some(project) = project {
            cmd.args([
                "--project",
                project.to_str().expect("utf8"),
                "--scope",
                "project",
            ]);
        }
        cmd.args(["--text", text]).assert().success();
    }

    fn refresh(&self) {
        cargo_bin_cmd!("hm")
            .args(["--config", self.config_str(), "refresh", "--force"])
            .assert()
            .success();
    }

    fn config_str(&self) -> &str {
        self.config.to_str().expect("utf8 config")
    }

    fn project(&self, repo: &Path) -> String {
        repo.join("src/main.rs").display().to_string()
    }

    /// Run `hm hook <args> --json` for `session` and parse the response.
    fn hook(&self, session: &str, args: &[&str]) -> Value {
        let output = cargo_bin_cmd!("hm")
            .env("HIVE_MEMORY_SESSION_ID", session)
            .args([
                "--config",
                self.config_str(),
                "--as-agent",
                "claude",
                "hook",
            ])
            .args(args)
            .arg("--json")
            .output()
            .expect("run hook");
        assert!(
            output.status.success(),
            "hook {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("hook JSON")
    }

    fn session_start(&self, session: &str, repo: &Path, extra: &[&str]) -> Value {
        let project = self.project(repo);
        let mut args = vec!["session-start", "--project", project.as_str()];
        args.extend_from_slice(extra);
        self.hook(session, &args)
    }

    fn prompt(&self, session: &str, repo: &Path, text: &str, extra: &[&str]) -> Value {
        let project = self.project(repo);
        let mut args = vec![
            "prompt-submit",
            "--project",
            project.as_str(),
            "--text",
            text,
        ];
        args.extend_from_slice(extra);
        self.hook(session, &args)
    }

    fn state_json(&self, session: &str) -> Value {
        let path = self.state.join(format!("runs/{session}/hook-state.json"));
        serde_json::from_str(&fs::read_to_string(path).expect("state file")).expect("state JSON")
    }
}

fn context_body(response: &Value) -> Option<String> {
    response["actions"]
        .as_array()
        .expect("actions")
        .iter()
        .find(|action| action["kind"] == "inject_context")
        .map(|action| action["body"].as_str().expect("body").to_owned())
}

fn emitted_ids(response: &Value) -> Vec<String> {
    response["delivery"]["emitted_ids"]
        .as_array()
        .expect("emitted ids")
        .iter()
        .map(|id| id.as_str().expect("id").to_owned())
        .collect()
}

fn delivered_ids(state: &Value) -> Vec<String> {
    state["delivered"]
        .as_array()
        .map(|memories| {
            memories
                .iter()
                .map(|memory| memory["id"].as_str().expect("id").to_owned())
                .collect()
        })
        .unwrap_or_default()
}

const PERSISTENT: &[&str] = &["--delivery", "persistent"];

/// Memory block ids in an injected body (pointer blocks included).
/// Sets `mode` on a directory for one test and restores 0755 on drop, even
/// when the test panics.
struct Restricted<'a>(&'a Path);

impl<'a> Restricted<'a> {
    /// Returns `None` when mode bits do not restrict this user (root in a CI
    /// container), so callers skip instead of asserting on an open fixture.
    fn new(path: &'a Path, mode: u32) -> Option<Self> {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
        let guard = Self(path);
        let probe = path.join(".mode-probe");
        let writable = fs::write(&probe, b"").is_ok();
        let _ = fs::remove_file(&probe);
        let readable = fs::read_dir(path).is_ok();
        let restricted = (mode & 0o200 == 0 && !writable) || (mode & 0o400 == 0 && !readable);
        restricted.then_some(guard)
    }
}

impl Drop for Restricted<'_> {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(self.0, fs::Permissions::from_mode(0o755));
    }
}

fn memory_count(body: &str) -> usize {
    body.matches("<memory id=").count()
}

/// Memory blocks in `body` as (id, block text), in order.
fn memory_blocks(body: &str) -> Vec<(String, String)> {
    let mut blocks = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("<memory id=\"") {
        let after = &rest[start..];
        let end = after.find("</memory>\n").expect("closed block") + "</memory>\n".len();
        let end = if after[end..].starts_with('\n') {
            end + 1
        } else {
            end
        };
        let block = &after[..end];
        let id = block["<memory id=\"".len()..]
            .split('"')
            .next()
            .expect("id")
            .to_owned();
        blocks.push((id, block.to_owned()));
        rest = &after[end..];
    }
    blocks
}

#[test]
fn legacy_hooks_keep_their_json_contract() {
    let fx = Fixture::new("delivery-legacy");
    let start = fx.session_start("legacy", &fx.repo_a, &[]);
    assert!(start.get("ack").is_none());
    assert!(start.get("delivery").is_none());
    let body = context_body(&start).expect("startup context");
    assert!(body.contains(GLOBAL_A) && body.contains(GLOBAL_B));

    let state = fx.state_json("legacy");
    assert!(state.get("delivered").is_none());
    assert_eq!(
        state["startup_memory_ids"].as_array().expect("ids").len(),
        2
    );
}

#[test]
fn returning_to_an_earlier_project_resends_nothing_visible() {
    let fx = Fixture::new("delivery-switch");
    let start = fx.session_start("switch", &fx.repo_a, PERSISTENT);
    assert_eq!(start["delivery"]["lifetime"], "persistent");
    assert_eq!(emitted_ids(&start).len(), 2);
    assert_eq!(delivered_ids(&fx.state_json("switch")).len(), 2);

    // Project B adds one project memory; the globals are already visible.
    let to_b = fx.prompt("switch", &fx.repo_b, "Inspect the tests.", PERSISTENT);
    let body = context_body(&to_b).expect("project B context");
    assert!(body.contains(PROJECT_B));
    assert!(!body.contains(GLOBAL_A) && !body.contains(GLOBAL_B));
    assert_eq!(to_b["delivery"]["skipped_delivered"], 2);

    // Back to A: everything is visible, so only the header marks the switch.
    let back = fx.prompt("switch", &fx.repo_a, "Inspect the tests.", PERSISTENT);
    let header = context_body(&back).expect("switch header");
    assert!(header.contains("Hive Memory Context"));
    assert_eq!(memory_count(&header), 0);
    assert_eq!(back["delivery"]["skipped_delivered"], 2);
    assert_eq!(delivered_ids(&fx.state_json("switch")).len(), 3);

    // The legacy path re-sends the whole block on the same switch.
    fx.session_start("switch-legacy", &fx.repo_a, &[]);
    fx.prompt("switch-legacy", &fx.repo_b, "Inspect the tests.", &[]);
    let legacy_back = fx.prompt("switch-legacy", &fx.repo_a, "Inspect the tests.", &[]);
    let legacy_body = context_body(&legacy_back).expect("legacy re-injection");
    assert!(legacy_body.contains(GLOBAL_A));
}

#[test]
fn context_reset_event_makes_the_next_prompt_resend_context() {
    let fx = Fixture::new("delivery-reset-event");
    fx.session_start("reset", &fx.repo_a, PERSISTENT);

    assert!(
        fx.state_json("reset")
            .get("delivered_context_key")
            .is_some()
    );
    let reset = fx.hook("reset", &["context-reset"]);
    assert_eq!(reset["event"], "context-reset");
    let state = fx.state_json("reset");
    assert!(state.get("delivered_context_key").is_none());
    assert!(delivered_ids(&state).is_empty());
    assert_eq!(state["context_epoch"], 1);

    let next = fx.prompt("reset", &fx.repo_a, "Inspect the tests.", PERSISTENT);
    let body = context_body(&next).expect("context re-sent after reset");
    assert!(body.contains(GLOBAL_A) && body.contains(GLOBAL_B));
}

#[test]
fn session_start_reset_flag_resends_after_compaction() {
    let fx = Fixture::new("delivery-reset-flag");
    fx.session_start("compact", &fx.repo_a, PERSISTENT);

    // A repeated startup without a reset sends no memory again.
    let repeat = fx.session_start("compact", &fx.repo_a, PERSISTENT);
    assert_eq!(memory_count(&context_body(&repeat).unwrap_or_default()), 0);
    assert_eq!(repeat["delivery"]["skipped_delivered"], 2);

    let compacted = fx.session_start(
        "compact",
        &fx.repo_a,
        &["--delivery", "persistent", "--reset"],
    );
    assert_eq!(compacted["delivery"]["reset"], true);
    assert_eq!(emitted_ids(&compacted).len(), 2);
}

#[test]
fn prompt_reset_flag_resends_context() {
    let fx = Fixture::new("delivery-prompt-reset");
    fx.session_start("dirty", &fx.repo_a, PERSISTENT);
    let next = fx.prompt(
        "dirty",
        &fx.repo_a,
        "Inspect the tests.",
        &["--delivery", "persistent", "--reset"],
    );
    let body = context_body(&next).expect("context after reset flag");
    assert!(body.contains(GLOBAL_A));
}

#[test]
fn emitted_memories_count_as_delivered_without_an_acknowledgement() {
    let fx = Fixture::new("delivery-on-emit");
    let start = fx.session_start("emit", &fx.repo_a, PERSISTENT);
    assert!(start.get("ack").is_none());
    assert_eq!(delivered_ids(&fx.state_json("emit")).len(), 2);
    let next = fx.prompt("emit", &fx.repo_a, "Inspect the tests.", PERSISTENT);
    assert!(context_body(&next).is_none());
}

#[test]
fn failed_reset_fails_the_hook() {
    let fx = Fixture::new("delivery-reset-fails");
    fx.session_start("locked", &fx.repo_a, PERSISTENT);
    // An unwritable session state dir makes the reset's state write fail.
    let run = fx.state.join("runs/locked");
    let Some(lock) = Restricted::new(&run, 0o500) else {
        eprintln!("skipped: mode bits do not restrict this user (root?)");
        return;
    };
    let project = fx.project(&fx.repo_a);
    for args in [
        vec![
            "session-start",
            "--project",
            project.as_str(),
            "--delivery",
            "persistent",
            "--reset",
        ],
        vec!["context-reset"],
    ] {
        let output = cargo_bin_cmd!("hm")
            .env("HIVE_MEMORY_SESSION_ID", "locked")
            .args(["--config", fx.config_str(), "--as-agent", "claude", "hook"])
            .args(&args)
            .arg("--json")
            .output()
            .expect("run hook");
        // Not 0 (adapters keep the reset pending) and not 2 (adapters read 2
        // as an older hm and retry untracked).
        assert_eq!(output.status.code(), Some(1), "{args:?}");
    }
    drop(lock);
    assert_eq!(delivered_ids(&fx.state_json("locked")).len(), 2);
}

#[test]
fn size_limit_marks_only_what_fits() {
    let fx = Fixture::new("delivery-cap");
    let full = fx.session_start("full", &fx.repo_a, PERSISTENT);
    let full_body = context_body(&full).expect("full body");
    let one_less = full_body.len() - 1;
    let cap = one_less.to_string();

    let capped = fx.session_start(
        "capped",
        &fx.repo_a,
        &[
            "--delivery",
            "persistent",
            "--max-context-bytes",
            cap.as_str(),
        ],
    );
    let body = context_body(&capped).expect("capped body");
    assert!(body.len() <= one_less);
    assert_eq!(emitted_ids(&capped).len(), 1);
    assert_eq!(capped["delivery"]["skipped_over_budget"], 1);
    let state = fx.state_json("capped");
    assert_eq!(delivered_ids(&state), emitted_ids(&capped));

    // The dropped memory stays eligible: recall can still surface it.
    let missing = if body.contains(GLOBAL_A) {
        "bravo"
    } else {
        "alpha"
    };
    let recall = fx.prompt(
        "capped",
        &fx.repo_a,
        &format!("delivery fact {missing}"),
        &[
            "--delivery",
            "persistent",
            "--max-context-bytes",
            cap.as_str(),
        ],
    );
    let recalled = context_body(&recall).expect("recalled dropped memory");
    assert!(recalled.contains(&format!("fact {missing}")));
}

#[test]
fn size_limit_below_header_emits_nothing_and_retries() {
    let fx = Fixture::new("delivery-tiny-cap");
    let tiny = fx.session_start(
        "tiny",
        &fx.repo_a,
        &["--delivery", "persistent", "--max-context-bytes", "10"],
    );
    assert_eq!(tiny["context_emitted"], false);
    assert!(context_body(&tiny).is_none());

    // Nothing was delivered and the selection was not recorded, so a prompt
    // with room sends the startup context.
    let next = fx.prompt("tiny", &fx.repo_a, "Inspect the tests.", PERSISTENT);
    assert!(context_body(&next).expect("context").contains(GLOBAL_A));
}

#[test]
fn legacy_size_limit_records_only_emitted_ids() {
    let fx = Fixture::new("delivery-legacy-cap");
    let full = fx.session_start("full", &fx.repo_a, &[]);
    let cap = (context_body(&full).expect("body").len() - 1).to_string();
    let capped = fx.session_start(
        "legacy-capped",
        &fx.repo_a,
        &["--max-context-bytes", cap.as_str()],
    );
    let body = context_body(&capped).expect("capped body");
    assert!(body.len() < cap.parse::<usize>().expect("cap") + 1);
    let state = fx.state_json("legacy-capped");
    assert_eq!(
        state["startup_memory_ids"].as_array().expect("ids").len(),
        1
    );
}

#[test]
fn turn_lifetime_never_suppresses_later_turns() {
    let fx = Fixture::new("delivery-turn");
    let turn: &[&str] = &["--delivery", "turn"];
    fx.session_start("turn", &fx.repo_a, PERSISTENT);
    fx.hook("turn", &["context-reset"]);

    // After a reset the context change re-sends; with turn lifetime nothing
    // joins the delivered set.
    let first = fx.prompt("turn", &fx.repo_b, "zephyrine release checklist", turn);
    assert!(
        context_body(&first)
            .expect("first turn")
            .contains(PROJECT_B)
    );
    assert!(delivered_ids(&fx.state_json("turn")).is_empty());

    // Same query next turn: like untracked recall, it skips what the current
    // selection carried.
    let second = fx.prompt("turn", &fx.repo_b, "zephyrine release checklist", turn);
    assert!(
        !context_body(&second)
            .unwrap_or_default()
            .contains(PROJECT_B)
    );
    // Turn memories never join the delivered set, so a later change back to
    // the project sends them again.
    fx.prompt("turn", &fx.repo_a, "Inspect the tests.", turn);
    let back = fx.prompt("turn", &fx.repo_b, "Inspect the tests.", turn);
    assert!(context_body(&back).expect("back to B").contains(PROJECT_B));
}

impl Fixture {
    /// Remember a reference memory: search-only at session start, so only
    /// prompt recall shows it.
    fn remember_reference(&self, text: &str) {
        cargo_bin_cmd!("hm")
            .args([
                "--config",
                self.config_str(),
                "--as-agent",
                "claude",
                "remember",
                "--kind",
                "reference",
                "--text",
                text,
            ])
            .assert()
            .success();
    }
}

#[test]
fn turn_recall_moves_past_hits_it_already_showed() {
    let fx = Fixture::with_strategy("delivery-turn-advance", "relevance");
    for step in 1..=7 {
        fx.remember_reference(&format!("Quasar beacon calibration step {step} of seven."));
    }
    fx.refresh();
    let turn: &[&str] = &["--delivery", "turn"];
    // The first turn sends the project's context selection (if any) instead
    // of recall. The references are search-only, so it never carries them.
    let first = fx.prompt("advance", &fx.repo_a, "Inspect the tests.", turn);
    assert!(!context_body(&first).unwrap_or_default().contains("Quasar"));

    // Like untracked recall, each turn moves on to the next hits, so all
    // seven appear across three turns of the same question.
    let mut seen = Vec::new();
    for _ in 0..3 {
        let response = fx.prompt("advance", &fx.repo_a, "quasar beacon calibration", turn);
        let ids = emitted_ids(&response);
        assert!(!ids.is_empty());
        for id in ids {
            assert!(!seen.contains(&id), "{id} repeated before the rest");
            seen.push(id);
        }
    }
    assert_eq!(seen.len(), 7);
    // Turn recall never joins the delivered set: the host dropped it.
    assert!(
        delivered_ids(&fx.state_json("advance"))
            .iter()
            .all(|id| !seen.contains(id))
    );
    // Once every hit was shown, the top hits come back rather than nothing.
    let again = fx.prompt("advance", &fx.repo_a, "quasar beacon calibration", turn);
    assert!(!emitted_ids(&again).is_empty());
    // A reset starts over.
    fx.hook("advance", &["context-reset"]);
    assert!(fx.state_json("advance").get("turn_recall_ids").is_none());
}

#[test]
fn recall_envelope_counts_raw_bytes_while_the_host_cap_counts_json() {
    let fx = Fixture::with_strategy("delivery-recall-envelope", "relevance");
    // Three quote-heavy hits: about 1.4 KB each raw, twice that escaped, so
    // together they fit recall's raw per-prompt envelope (4,800 bytes at the
    // default token budget) but not once escaped.
    for name in ["one", "two", "three"] {
        fx.remember_reference(&format!(
            "Nebula quoting rule {name}: {}",
            "\"q\" ".repeat(300)
        ));
        // Plain hits of about 2 KB: three exceed the raw envelope.
        fx.remember_reference(&format!(
            "Comet survey note {name}: {}",
            "plain ".repeat(330)
        ));
    }
    fx.refresh();
    let json_cap: &[&str] = &[
        "--delivery",
        "turn",
        "--max-context-bytes",
        "16000",
        "--context-size",
        "json",
    ];
    // The first turn sends the context selection; the second runs recall.
    fx.prompt("envelope", &fx.repo_a, "Inspect the tests.", json_cap);
    let response = fx.prompt("envelope", &fx.repo_a, "nebula quoting rule", json_cap);
    assert_eq!(emitted_ids(&response).len(), 3, "{response}");
    assert_eq!(response["delivery"]["skipped_over_budget"], 0);
    // The raw envelope still bounds one prompt's recall, as untracked.
    let comets = fx.prompt("envelope", &fx.repo_a, "comet survey note", json_cap);
    assert_eq!(emitted_ids(&comets).len(), 2, "{comets}");
    assert_eq!(comets["delivery"]["skipped_over_budget"], 1);
    // A hit larger than the envelope alone, though under the host cap,
    // becomes a pointer rather than stopping recall.
    fx.remember_reference(&format!("Aurora field log: {}", "plain ".repeat(1000)));
    fx.refresh();
    let aurora = fx.prompt("envelope", &fx.repo_a, "aurora field log", json_cap);
    assert_eq!(aurora["delivery"]["omitted"], 1, "{aurora}");
    assert!(
        context_body(&aurora)
            .expect("pointer")
            .contains("omitted=\"too-large\"")
    );

    // The host cap itself is counted escaped: room for the hits raw is not
    // room once escaped.
    let raw_len = context_body(&response).expect("recall").len();
    let tight = (raw_len + 100).to_string();
    let tight_cap: &[&str] = &[
        "--delivery",
        "turn",
        "--max-context-bytes",
        tight.as_str(),
        "--context-size",
        "json",
    ];
    fx.prompt("tight", &fx.repo_a, "Inspect the tests.", tight_cap);
    let capped = fx.prompt("tight", &fx.repo_a, "nebula quoting rule", tight_cap);
    let body = context_body(&capped).expect("capped recall");
    assert!(serde_json::to_string(&body).expect("json").len() - 2 <= raw_len + 100);
    assert!(emitted_ids(&capped).len() < 3);
}

#[test]
fn muse_prompts_equal_untracked_output_minus_session_start_memories() {
    // The one designed difference from untracked hooks on Muse: prompt
    // context leaves out exactly the memories session start already put in
    // the context Muse keeps. Pin it so it cannot grow silently.
    let fx = Fixture::new("delivery-muse-equivalence");
    for step in 1..=3 {
        fx.remember(
            &format!("Zephyrine release checklist step {step} for project bravo."),
            Some(&fx.repo_b.clone()),
        );
    }
    fx.refresh();
    // Warm the rendered cache so both sessions render the same freshness note.
    fx.session_start("warm", &fx.repo_a, &[]);
    fx.session_start("warm", &fx.repo_b, &[]);

    let legacy_start = fx.session_start("legacy", &fx.repo_a, &[]);
    let tracked_start = fx.session_start("tracked", &fx.repo_a, PERSISTENT);
    let delivered = memory_blocks(&context_body(&tracked_start).expect("tracked start"))
        .into_iter()
        .map(|(id, _)| id)
        .collect::<Vec<_>>();
    assert_eq!(
        context_body(&legacy_start),
        context_body(&tracked_start),
        "session start is unchanged"
    );
    let turn: &[&str] = &["--delivery", "turn"];

    let prompts: [(&Path, &str); 3] = [
        (&fx.repo_b, "Inspect the tests."),
        (&fx.repo_b, "zephyrine release checklist"),
        (&fx.repo_a, "delivery fact alpha"),
    ];
    for (repo, text) in prompts {
        let legacy = context_body(&fx.prompt("legacy", repo, text, &[])).unwrap_or_default();
        let tracked = context_body(&fx.prompt("tracked", repo, text, turn)).unwrap_or_default();
        let mut expected = legacy.clone();
        for (id, block) in memory_blocks(&legacy) {
            if delivered.contains(&id) {
                expected = expected.replacen(&block, "", 1);
            }
        }
        if legacy.is_empty() {
            assert!(tracked.is_empty(), "{text}: {tracked}");
            continue;
        }
        if memory_blocks(&expected).is_empty()
            && memory_blocks(&legacy)
                .iter()
                .all(|(id, _)| delivered.contains(id))
            && !legacy.contains("Hive Memory Context")
        {
            // Recall carrying only session-start memories sends nothing.
            assert!(tracked.is_empty(), "{text}: {tracked}");
            continue;
        }
        assert_eq!(tracked, expected, "{text}");
    }
}

#[test]
fn turn_recall_offers_the_previous_projects_memories_after_a_switch() {
    // Turn recall skips only the current selection: once the session moves
    // from A to B, A's selection is gone from view and recall offers its
    // memories again, as untracked recall does.
    let fx = Fixture::new("delivery-turn-switch-recall");
    fx.remember(
        "Project alpha uses the quillon deploy runbook.",
        Some(&fx.repo_a.clone()),
    );
    fx.refresh();
    let turn: &[&str] = &["--delivery", "turn"];
    let a = fx.prompt("switch", &fx.repo_a, "Inspect the tests.", turn);
    assert!(context_body(&a).expect("A selection").contains("quillon"));
    let in_a = fx.prompt("switch", &fx.repo_a, "quillon deploy runbook", turn);
    assert!(!context_body(&in_a).unwrap_or_default().contains("quillon"));
    let b = fx.prompt("switch", &fx.repo_b, "Inspect the tests.", turn);
    assert!(!context_body(&b).expect("B selection").contains("quillon"));
    let legacy_a = fx.prompt("legacy", &fx.repo_a, "Inspect the tests.", &[]);
    assert!(context_body(&legacy_a).is_some());
    fx.prompt("legacy", &fx.repo_b, "Inspect the tests.", &[]);
    let legacy = fx.prompt("legacy", &fx.repo_b, "quillon deploy runbook", &[]);
    let tracked = fx.prompt("switch", &fx.repo_b, "quillon deploy runbook", turn);
    assert_eq!(
        context_body(&tracked)
            .unwrap_or_default()
            .contains("quillon"),
        context_body(&legacy)
            .unwrap_or_default()
            .contains("quillon"),
        "tracked {tracked} legacy {legacy}"
    );
    assert!(
        context_body(&tracked)
            .unwrap_or_default()
            .contains("quillon")
    );
}

#[test]
fn turn_recall_skips_persistent_session_start_memories() {
    let fx = Fixture::new("delivery-turn-startup");
    fx.session_start("mixed", &fx.repo_b, PERSISTENT);
    let recall = fx.prompt(
        "mixed",
        &fx.repo_b,
        "zephyrine release checklist",
        &["--delivery", "turn"],
    );
    // Session-start context persists, so turn recall does not repeat it.
    assert!(context_body(&recall).is_none());
}

#[test]
fn persistent_recall_skips_delivered_memories() {
    let fx = Fixture::new("delivery-recall");
    fx.session_start("recall", &fx.repo_a, PERSISTENT);
    fx.hook("recall", &["context-reset"]);
    fx.prompt("recall", &fx.repo_a, "Inspect the tests.", PERSISTENT);

    let recall = fx.prompt("recall", &fx.repo_a, "delivery fact alpha", PERSISTENT);
    assert!(context_body(&recall).is_none());
    assert_eq!(recall["recall"]["reason"], "unchanged");
}

#[test]
fn changed_text_under_the_same_id_is_new() {
    let fx = Fixture::new("delivery-digest");
    fx.session_start("digest", &fx.repo_a, PERSISTENT);
    let path = fx.state.join("runs/digest/hook-state.json");
    let mut state = fx.state_json("digest");
    for memory in state["delivered"].as_array_mut().expect("delivered") {
        memory["digest"] = Value::from("text-delivered-earlier");
    }
    fs::write(&path, serde_json::to_vec(&state).expect("json")).expect("rewrite digests");

    let repeat = fx.session_start("digest", &fx.repo_a, PERSISTENT);
    assert_eq!(emitted_ids(&repeat).len(), 2);
}

#[test]
fn delivery_without_session_id_falls_back_to_legacy() {
    let fx = Fixture::new("delivery-no-session");
    let output = cargo_bin_cmd!("hm")
        .args([
            "--config",
            fx.config_str(),
            "--as-agent",
            "claude",
            "hook",
            "session-start",
            "--delivery",
            "persistent",
            "--json",
        ])
        .output()
        .expect("run hook");
    assert!(output.status.success());
    let response: Value = serde_json::from_slice(&output.stdout).expect("json");
    assert!(response.get("delivery").is_none());
    assert!(context_body(&response).is_some());
}

#[test]
fn context_reset_without_session_id_is_a_warning_only() {
    let fx = Fixture::new("delivery-reset-no-session");
    let output = cargo_bin_cmd!("hm")
        .args([
            "--config",
            fx.config_str(),
            "hook",
            "context-reset",
            "--json",
        ])
        .output()
        .expect("run hook");
    assert!(output.status.success());
    let response: Value = serde_json::from_slice(&output.stdout).expect("json");
    assert_eq!(response["warnings"].as_array().expect("warnings").len(), 1);
}

#[test]
fn reset_requires_delivery_tracking() {
    let fx = Fixture::new("delivery-flag-guard");
    let flags: [&[&str]; 3] = [&["--reset"], &["--ack"], &["--context-size", "json"]];
    for flag in flags {
        let output = cargo_bin_cmd!("hm")
            .env("HIVE_MEMORY_SESSION_ID", "guard")
            .args(["--config", fx.config_str(), "hook", "session-start"])
            .args(flag)
            .output()
            .expect("run hook");
        // Usage errors exit 2 (`--ack` no longer exists); adapters rely on
        // that to detect an hm that does not know a flag.
        assert_eq!(output.status.code(), Some(2), "{flag:?}");
    }
}

#[test]
fn turn_lifetime_sends_a_project_switch_once_per_change() {
    let fx = Fixture::new("delivery-turn-neutral");
    let turn: &[&str] = &["--delivery", "turn"];
    fx.session_start("neutral", &fx.repo_a, PERSISTENT);

    // Like untracked hooks, a turn-lifetime selection goes out once per
    // change (prompt recall still runs every turn); session-start memories
    // persist, so they are not repeated.
    let first = fx.prompt("neutral", &fx.repo_b, "Inspect the tests.", turn);
    let body = context_body(&first).expect("switch to B");
    assert!(body.contains(PROJECT_B));
    assert!(!body.contains(GLOBAL_A));
    for _ in 0..2 {
        let later = fx.prompt("neutral", &fx.repo_b, "Inspect the tests.", turn);
        assert!(context_body(&later).is_none(), "{later}");
    }
    // A later switch away and back sends it again.
    fx.prompt("neutral", &fx.repo_a, "Inspect the tests.", turn);
    let back = fx.prompt("neutral", &fx.repo_b, "Inspect the tests.", turn);
    assert!(context_body(&back).expect("back to B").contains(PROJECT_B));
    assert!(delivered_ids(&fx.state_json("neutral")).len() == 2);
}

#[test]
fn turn_lifetime_switch_follows_the_untracked_order_within_the_cap() {
    let fx = Fixture::new("delivery-turn-recall");
    for index in 0..6 {
        fx.remember(
            &format!("Bravo checklist step {index} for the zephyrine release."),
            Some(&fx.repo_b.clone()),
        );
    }
    fx.refresh();
    fx.session_start("turn-recall", &fx.repo_a, PERSISTENT);
    let cap = "900";
    let turn: &[&str] = &["--delivery", "turn", "--max-context-bytes", cap];

    // The switch sends project B's selection (fitted to the cap) instead of
    // recall, as untracked hooks do.
    let switch = fx.prompt(
        "turn-recall",
        &fx.repo_b,
        "zephyrine release checklist",
        turn,
    );
    let body = context_body(&switch).expect("switch context");
    assert!(body.len() <= 900, "{}", body.len());
    assert_eq!(switch["recall"]["reason"], "context-selection-changed");
    // Next turn recall runs, also within the cap, and nothing turn-lived was
    // marked delivered.
    let next = fx.prompt(
        "turn-recall",
        &fx.repo_b,
        "zephyrine release checklist",
        turn,
    );
    let recalled = context_body(&next).expect("recall next turn");
    assert!(recalled.len() <= 900, "{}", recalled.len());
    assert!(recalled.contains("zephyrine"));
    assert_eq!(delivered_ids(&fx.state_json("turn-recall")).len(), 2);
}

#[test]
fn size_limit_remainder_drains_on_later_prompts() {
    let fx = Fixture::new("delivery-drain");
    for index in 0..4 {
        fx.remember(
            &format!("Drain fact {index} keeps the remaining startup memories reachable."),
            None,
        );
    }
    fx.refresh();
    let full = fx.session_start("drain-full", &fx.repo_a, PERSISTENT);
    let total = emitted_ids(&full).len();
    assert_eq!(total, 6);
    // Room for the header and about half of the memories.
    let cap = (context_body(&full).expect("body").len() / 2).to_string();
    let capped: &[&str] = &[
        "--delivery",
        "persistent",
        "--max-context-bytes",
        cap.as_str(),
    ];

    let mut delivered = emitted_ids(&fx.session_start("drain", &fx.repo_a, capped));
    assert!(delivered.len() < total);
    // Neutral prompts in the same project keep sending the rest, a chunk at a
    // time, with no repeats, until everything is visible.
    for _ in 0..total {
        let response = fx.prompt("drain", &fx.repo_a, "Inspect the tests.", capped);
        if context_body(&response).is_none() {
            break;
        }
        for id in emitted_ids(&response) {
            assert!(!delivered.contains(&id), "{id} sent twice");
            delivered.push(id);
        }
    }
    assert_eq!(delivered.len(), total);
    let settled = fx.prompt("drain", &fx.repo_a, "Inspect the tests.", capped);
    assert!(context_body(&settled).is_none());
    assert!(
        fx.state_json("drain")
            .get("delivered_context_key")
            .is_some()
    );
}

#[test]
fn untracked_hooks_on_the_same_session_cannot_suppress_tracked_resends() {
    let fx = Fixture::new("delivery-mixed");
    // After a context loss, untracked hooks on the same session id (a nested
    // agent or an older adapter) record their own context and recall
    // selections before the tracked session prompts again.
    fx.session_start("mixed", &fx.repo_a, PERSISTENT);
    fx.hook("mixed", &["context-reset"]);
    fx.prompt("mixed", &fx.repo_a, "Inspect the tests.", &[]);
    fx.prompt("mixed", &fx.repo_a, "zephyrine release checklist", &[]);
    let state = fx.state_json("mixed");
    assert!(state.get("context_key").is_some());
    assert!(state.get("prompt_recall_key").is_some());
    assert!(state.get("delivered_context_key").is_none());

    // The tracked session still re-sends what it lost.
    let next = fx.prompt(
        "mixed",
        &fx.repo_a,
        "zephyrine release checklist",
        PERSISTENT,
    );
    let body = context_body(&next).expect("tracked re-send");
    assert!(body.contains(GLOBAL_A) && body.contains(GLOBAL_B));
}

#[test]
fn an_untracked_fallback_starts_from_what_tracked_hooks_sent() {
    // Adapters call untracked when there is too little room. Untracked hooks
    // must then behave as if every earlier hook had been untracked: no re-send
    // of the selection the session already has, no repeat of recall.
    let fx = Fixture::new("delivery-fallback");
    fx.session_start("fallback", &fx.repo_a, PERSISTENT);
    let plain = fx.prompt("fallback", &fx.repo_a, "Inspect the tests.", &[]);
    assert!(context_body(&plain).is_none(), "{plain}");

    let tracked = fx.prompt(
        "fallback",
        &fx.repo_b,
        "zephyrine release checklist",
        PERSISTENT,
    );
    assert!(context_body(&tracked).expect("switch").contains(PROJECT_B));
    let again = fx.prompt("fallback", &fx.repo_b, "zephyrine release checklist", &[]);
    assert!(context_body(&again).is_none(), "{again}");

    // Turn lifetime mirrors the same way, for the selection and for recall.
    let fx = Fixture::with_strategy("delivery-fallback-turn", "relevance");
    fx.remember_reference("Orbit reference note for the fallback.");
    fx.refresh();
    let turn: &[&str] = &["--delivery", "turn"];
    fx.prompt("fallback-turn", &fx.repo_b, "Inspect the tests.", turn);
    let plain = fx.prompt("fallback-turn", &fx.repo_b, "Inspect the tests.", &[]);
    assert!(context_body(&plain).is_none(), "{plain}");
    let recall = fx.prompt("fallback-turn", &fx.repo_b, "orbit reference note", turn);
    assert!(
        context_body(&recall)
            .expect("turn recall")
            .contains("Orbit reference note")
    );
    let untracked = fx.prompt("fallback-turn", &fx.repo_b, "orbit reference note", &[]);
    assert!(
        !context_body(&untracked)
            .unwrap_or_default()
            .contains("Orbit reference note"),
        "{untracked}"
    );
}

#[test]
fn an_untracked_fallback_still_sends_a_selection_cut_by_the_cap() {
    let fx = Fixture::new("delivery-fallback-cut");
    let full = fx.session_start("cut-full", &fx.repo_a, PERSISTENT);
    let cap = (context_body(&full).expect("body").len() - 1).to_string();
    let capped: &[&str] = &["--delivery", "persistent", "--max-context-bytes", &cap];
    let start = fx.session_start("cut", &fx.repo_a, capped);
    assert!(
        start["delivery"]["skipped_over_budget"]
            .as_u64()
            .expect("count")
            > 0
    );
    // Untracked hooks would have sent everything at start, so the fallback
    // sends the selection rather than treating it as received.
    let fallback = fx.prompt("cut", &fx.repo_a, "Inspect the tests.", &[]);
    let body = context_body(&fallback).expect("fallback re-sends the selection");
    assert!(body.contains(GLOBAL_A) && body.contains(GLOBAL_B), "{body}");
}

#[test]
fn untracked_recall_cannot_suppress_tracked_recall() {
    let fx = Fixture::new("delivery-mixed-recall");
    fx.session_start("mixed-recall", &fx.repo_a, PERSISTENT);
    // Project B's memory is outside A's startup selection, so only recall
    // shows it. An untracked recall of the same query records its key first
    // (the first untracked prompt sends its own startup context).
    fx.prompt("mixed-recall", &fx.repo_a, "Inspect the tests.", &[]);
    fx.prompt(
        "mixed-recall",
        &fx.repo_a,
        "zephyrine release checklist",
        &[],
    );
    assert!(
        fx.state_json("mixed-recall")
            .get("prompt_recall_key")
            .is_some()
    );
    let tracked = fx.prompt(
        "mixed-recall",
        &fx.repo_a,
        "zephyrine release checklist",
        PERSISTENT,
    );
    let body = context_body(&tracked).expect("tracked recall");
    assert!(body.contains(PROJECT_B));
}

#[test]
fn persistent_recall_repeats_changed_text_under_the_same_id() {
    let fx = Fixture::new("delivery-recall-digest");
    fx.session_start("recall-digest", &fx.repo_a, PERSISTENT);
    let path = fx.state.join("runs/recall-digest/hook-state.json");
    let mut state = fx.state_json("recall-digest");
    for memory in state["delivered"].as_array_mut().expect("delivered") {
        memory["digest"] = Value::from("text-delivered-earlier");
    }
    fs::write(&path, serde_json::to_vec(&state).expect("json")).expect("rewrite digests");

    let recall = fx.prompt(
        "recall-digest",
        &fx.repo_a,
        "delivery fact alpha",
        PERSISTENT,
    );
    let body = context_body(&recall).expect("changed text recalled");
    assert!(body.contains(GLOBAL_A), "{recall}");
}

#[test]
fn recall_respects_the_size_limit() {
    let fx = Fixture::new("delivery-recall-cap");
    let full = fx.session_start("recall-cap-full", &fx.repo_b, &[]);
    let header_len = context_body(&full)
        .expect("body")
        .find("<memory")
        .expect("memory block");
    // Room for the recall header but not one memory block.
    let cap = (header_len + 20).to_string();
    for (session, tracked) in [("recall-cap", true), ("recall-cap-untracked", false)] {
        let mut start: Vec<&str> = Vec::new();
        let mut flags = vec!["--max-context-bytes", cap.as_str()];
        if tracked {
            start.extend(["--delivery", "persistent"]);
            flags.extend(["--delivery", "persistent"]);
        }
        // Start in A so project B's memory is only reachable by recall.
        fx.session_start(session, &fx.repo_a, &start);
        let response = fx.prompt(session, &fx.repo_a, "zephyrine release checklist", &flags);
        assert!(context_body(&response).is_none(), "{session}: {response}");
    }
}

#[test]
fn untracked_prompt_context_change_respects_the_size_limit() {
    let fx = Fixture::new("delivery-untracked-cap");
    fx.session_start("untracked-cap-full", &fx.repo_a, &[]);
    let full = fx.prompt("untracked-cap-full", &fx.repo_b, "Inspect the tests.", &[]);
    let full_len = context_body(&full).expect("full").len();
    let cap = (full_len - 1).to_string();
    fx.session_start("untracked-cap", &fx.repo_a, &[]);
    let capped = fx.prompt(
        "untracked-cap",
        &fx.repo_b,
        "Inspect the tests.",
        &["--max-context-bytes", cap.as_str()],
    );
    let body = context_body(&capped).expect("capped context change");
    assert!(body.len() < full_len);
}

#[test]
fn oversized_memory_does_not_starve_the_rest() {
    let fx = Fixture::new("delivery-oversize");
    // The newest memory ranks first under recency and is larger than the cap.
    let huge = format!("Oversize quasar note {}", "padding ".repeat(250));
    fx.remember(&huge, None);
    fx.refresh();
    let capped: &[&str] = &["--delivery", "persistent", "--max-context-bytes", "1600"];

    let start = fx.session_start("oversize", &fx.repo_a, capped);
    let body = context_body(&start).expect("startup context");
    assert!(body.len() <= 1600);
    assert!(body.contains("omitted=\"too-large\""));
    assert!(body.contains(GLOBAL_A) && body.contains(GLOBAL_B));
    assert_eq!(start["delivery"]["omitted"], 1);
    assert_eq!(start["delivery"]["skipped_over_budget"], 0);
    // The pointer is not content: the memory is never marked delivered, and
    // the selection completes instead of retrying forever.
    let state = fx.state_json("oversize");
    assert_eq!(delivered_ids(&state).len(), 2);
    assert!(state.get("delivered_context_key").is_some());

    // Recall hitting the oversized memory points to it and still shows the
    // smaller hits ranked below it.
    fx.remember("Quasar telemetry dashboard lives in the ops wiki.", None);
    fx.refresh();
    let recall = fx.prompt("oversize", &fx.repo_a, "quasar", capped);
    let recalled = context_body(&recall).expect("recall");
    assert!(recalled.len() <= 1600);
    assert!(recalled.contains("omitted=\"too-large\""));
    assert!(recalled.contains("Quasar telemetry dashboard"));
}

#[test]
fn recall_still_runs_while_the_startup_remainder_drains() {
    let fx = Fixture::new("delivery-drain-recall");
    for index in 0..4 {
        fx.remember(
            &format!("Drain fact {index} keeps startup memories reachable."),
            None,
        );
    }
    fx.refresh();
    let full = fx.session_start("drain-recall-full", &fx.repo_a, PERSISTENT);
    let cap = (context_body(&full).expect("body").len() * 3 / 4).to_string();
    let capped: &[&str] = &[
        "--delivery",
        "persistent",
        "--max-context-bytes",
        cap.as_str(),
    ];
    let start = fx.session_start("drain-recall", &fx.repo_a, capped);
    assert!(
        start["delivery"]["skipped_over_budget"]
            .as_u64()
            .expect("count")
            > 0
    );

    // The remainder is still draining, and the prompt's own recall (project
    // B's memory, not part of A's startup selection) shares the response.
    let prompt = fx.prompt(
        "drain-recall",
        &fx.repo_a,
        "zephyrine release checklist",
        capped,
    );
    let body = context_body(&prompt).expect("drain plus recall");
    assert!(body.contains(PROJECT_B), "{prompt}");
    // Both parts together stay inside the host limit.
    assert!(
        body.len() <= cap.parse::<usize>().expect("cap"),
        "{}",
        body.len()
    );
    assert!(body.contains("Drain fact") || body.contains("Global delivery fact"));
    assert_ne!(prompt["recall"]["reason"], "context-selection-changed");
}

#[test]
fn recall_points_to_a_hit_larger_than_the_recall_budget() {
    let fx = Fixture::new("delivery-recall-oversize");
    fx.session_start("recall-big", &fx.repo_a, PERSISTENT);
    // Larger than the 1,200-token recall assembly budget.
    let huge = format!("Nebula throttling standard {}", "padding ".repeat(800));
    fx.remember(&huge, None);
    fx.remember("Nebula throttling dashboards live in the ops wiki.", None);
    fx.refresh();
    let capped: &[&str] = &["--delivery", "persistent", "--max-context-bytes", "3000"];
    let recall = fx.prompt("recall-big", &fx.repo_a, "nebula throttling", capped);
    let body = context_body(&recall).expect("recall with pointer");
    assert!(body.len() <= 3000);
    assert!(body.contains("omitted=\"too-large\""));
    assert!(body.contains("Nebula throttling dashboards"));
}

/// Run `hm get` as `agent` and return (exit code, stdout).
fn get(fx: &Fixture, agent: &str, args: &[&str]) -> (Option<i32>, String) {
    let output = cargo_bin_cmd!("hm")
        .args(["--config", fx.config_str(), "--as-agent", agent, "get"])
        .args(args)
        .output()
        .expect("run hm get");
    (
        output.status.code(),
        String::from_utf8(output.stdout).expect("utf8"),
    )
}

#[test]
fn pointer_names_an_hm_get_command_that_reads_the_memory() {
    let fx = Fixture::new("delivery-get");
    let huge = format!("Oversize orbit note {}", "padding ".repeat(300));
    fx.remember(&huge, None);
    fx.refresh();
    let start = fx.session_start(
        "get",
        &fx.repo_a,
        &["--delivery", "persistent", "--max-context-bytes", "1600"],
    );
    let body = context_body(&start).expect("startup context");
    let args = pointer_args(&body);
    assert_eq!(args[..2], ["--store", "personal"]);
    let id = args[2].as_str();
    assert!(
        !body.contains("/personal/"),
        "pointer must not name a store path"
    );

    let (code, text) = get(&fx, "claude", &args_ref(&args));
    assert_eq!(code, Some(0));
    assert!(text.contains(&format!("id: {id}")));
    assert!(text.contains(huge.trim_end()));
    let (code, json) = get(&fx, "claude", &[id, "--json"]);
    assert_eq!(code, Some(0));
    let json: Value = serde_json::from_str(&json).expect("json");
    assert_eq!(json["id"], id);
    assert!(
        json["body"]
            .as_str()
            .expect("body")
            .starts_with("Oversize orbit note")
    );
}

#[test]
fn hm_get_respects_audience_and_reports_unknown_ids() {
    let fx = Fixture::new("delivery-get-audience");
    let output = cargo_bin_cmd!("hm")
        .args([
            "--config",
            fx.config_str(),
            "--as-agent",
            "codex",
            "remember",
            "--scope",
            "agent-private",
            "--audience-writer-only",
            "--text",
            "Codex-private orbit fact.",
            "--json",
        ])
        .output()
        .expect("remember");
    assert!(output.status.success());
    let written: Value = serde_json::from_slice(&output.stdout).expect("json");
    let id = written["id"].as_str().expect("id").to_owned();
    fx.refresh();

    assert_eq!(get(&fx, "codex", &[&id]).0, Some(0));
    assert_eq!(get(&fx, "claude", &[&id]).0, Some(1));
    assert_eq!(get(&fx, "claude", &["no-such-memory"]).0, Some(1));
}

/// Arguments of the pointer command (`hm get --store <store> <id>`) for the
/// first omitted memory in `body`.
fn pointer_args(body: &str) -> Vec<String> {
    body.lines()
        .find_map(|line| line.split("read it with: hm get ").nth(1))
        .expect("pointer command")
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

fn args_ref(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

#[test]
fn hm_get_reads_curated_memories() {
    let fx = Fixture::new("delivery-get-curated");
    let store = fx.config.parent().expect("dir").join("personal");
    fs::create_dir_all(store.join("rules")).expect("rules dir");
    let mut big = String::from("# Big rules\n");
    for line in 0..200 {
        big.push_str(&format!(
            "- curated rule line {line} keeps padding the file\n"
        ));
    }
    fs::write(store.join("rules/big.md"), &big).expect("curated file");
    fx.refresh();
    let start = fx.session_start(
        "curated",
        &fx.repo_a,
        &["--delivery", "persistent", "--max-context-bytes", "1600"],
    );
    let args = pointer_args(&context_body(&start).expect("startup context"));
    assert_eq!(args, ["--store", "personal", "curated:rules/big.md"]);
    let (code, text) = get(&fx, "claude", &args_ref(&args));
    assert_eq!(code, Some(0), "{text}");
    assert!(text.contains("curated rule line 199"));
}

/// Readable stores `personal` (default), `archive` (empty), and `team`, with
/// `repo` bound to `team`. Returns the config path and the repo.
fn bound_store_fixture(name: &str) -> (PathBuf, PathBuf) {
    let dir = temp_dir(name);
    let config = dir.join("config.toml");
    let personal = dir.join("personal");
    let archive = dir.join("archive");
    let team = dir.join("team");
    let repo = dir.join("repo");
    fs::create_dir_all(&repo).expect("repo");
    fs::write(repo.join(".hive-memory-project"), "id = \"bound-repo\"\n").expect("marker");
    fs::write(
        &config,
        format!(
            r#"
            default_store = "personal"
            state_dir = "{state}"
            cache_dir = "{state}/cache"

            [stores.personal]
            root = "{personal}"

            [stores.archive]
            root = "{archive}"

            [stores.team]
            root = "{team}"

            [agents.claude]
            default_store = "personal"
            read_stores = ["personal", "archive", "team"]
            write_stores = ["personal", "team"]

            [defaults]
            context_strategy = "recency"
            "#,
            state = dir.join("state").display(),
            personal = personal.display(),
            archive = archive.display(),
            team = team.display()
        ),
    )
    .expect("config");
    let cfg = config.to_str().expect("utf8");
    for (name, root) in [
        ("personal", &personal),
        ("archive", &archive),
        ("team", &team),
    ] {
        cargo_bin_cmd!("hm")
            .args([
                "stores",
                "init",
                name,
                "--root",
                root.to_str().expect("utf8"),
            ])
            .assert()
            .success();
    }
    cargo_bin_cmd!("hm")
        .args([
            "--config",
            cfg,
            "projects",
            "bind",
            repo.to_str().expect("utf8"),
            "--store",
            "team",
        ])
        .assert()
        .success();
    (config, repo)
}

/// Run a delivery-tracked session start in `repo` with a small cap.
fn capped_start(cfg: &str, repo: &Path, session: &str) -> Value {
    let output = cargo_bin_cmd!("hm")
        .env("HIVE_MEMORY_SESSION_ID", session)
        .args([
            "--config",
            cfg,
            "--as-agent",
            "claude",
            "hook",
            "session-start",
            "--project",
            repo.to_str().expect("utf8"),
            "--delivery",
            "persistent",
            "--max-context-bytes",
            "1200",
            "--json",
        ])
        .output()
        .expect("session start");
    serde_json::from_slice(&output.stdout).expect("json")
}

#[test]
fn hm_get_follows_the_project_store_binding() {
    let (config, repo) = bound_store_fixture("delivery-get-bound");
    let cfg = config.to_str().expect("utf8");
    let huge = format!("Team orbit standard {}", "padding ".repeat(300));
    cargo_bin_cmd!("hm")
        .args([
            "--config",
            cfg,
            "--as-agent",
            "claude",
            "--store",
            "team",
            "remember",
            "--text",
            &huge,
        ])
        .assert()
        .success();
    cargo_bin_cmd!("hm")
        .args(["--config", cfg, "--store", "team", "refresh", "--force"])
        .assert()
        .success();
    let response = capped_start(cfg, &repo, "bound");
    let args = pointer_args(&context_body(&response).expect("startup context"));
    assert_eq!(args[..2], ["--store", "team"]);
    let id = args[2].as_str();

    // The pointer's command works as the model runs it. Without the store
    // flag, `--project` (or `HIVE_MEMORY_PROJECT`) applies the binding, and
    // with no hint at all the other readable stores are searched.
    let runs: [(&[&str], Option<&str>); 4] = [
        (&["--store", "team", id], None),
        (&["--project", repo.to_str().expect("utf8"), id], None),
        (&[id], Some(repo.to_str().expect("utf8"))),
        (&[id], None),
    ];
    for (args, project) in runs {
        let mut cmd = cargo_bin_cmd!("hm");
        if let Some(project) = project {
            cmd.env("HIVE_MEMORY_PROJECT", project);
        }
        let out = cmd
            .args(["--config", cfg, "--as-agent", "claude", "get"])
            .args(args)
            .output()
            .expect("hm get");
        assert_eq!(out.status.code(), Some(0), "{args:?} {project:?}");
        assert!(String::from_utf8_lossy(&out.stdout).contains("Team orbit standard"));
    }
}

#[test]
fn hm_get_skips_an_unreadable_fallback_store() {
    let (config, _repo) = bound_store_fixture("delivery-get-unreadable");
    let cfg = config.to_str().expect("utf8");
    let output = cargo_bin_cmd!("hm")
        .args([
            "--config",
            cfg,
            "--as-agent",
            "claude",
            "--store",
            "team",
            "remember",
            "--text",
            "Team fallback orbit fact.",
            "--json",
        ])
        .output()
        .expect("remember");
    let id = serde_json::from_slice::<Value>(&output.stdout).expect("json")["id"]
        .as_str()
        .expect("id")
        .to_owned();
    // `archive` is searched before `team` and has no cached index; its root
    // cannot be read.
    let archive = config.parent().expect("dir").join("archive");
    let Some(lock) = Restricted::new(&archive, 0o000) else {
        eprintln!("skipped: mode bits do not restrict this user (root?)");
        return;
    };
    let out = cargo_bin_cmd!("hm")
        .args(["--config", cfg, "--as-agent", "claude", "get", &id])
        .output();
    drop(lock);
    let out = out.expect("hm get");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("Team fallback orbit fact."));
    assert!(String::from_utf8_lossy(&out.stderr).contains("skipped store archive"));
}

#[test]
fn hm_get_pointer_reads_the_right_store_when_curated_ids_collide() {
    let (config, repo) = bound_store_fixture("delivery-get-collision");
    let cfg = config.to_str().expect("utf8");
    let root = config.parent().expect("dir");
    for store in ["personal", "team"] {
        let mut big = format!("# {store} rules\n");
        for line in 0..200 {
            big.push_str(&format!("- {store} rule line {line} pads the file\n"));
        }
        fs::create_dir_all(root.join(store).join("rules")).expect("rules dir");
        fs::write(root.join(store).join("rules/big.md"), big).expect("curated file");
        cargo_bin_cmd!("hm")
            .args(["--config", cfg, "--store", store, "refresh", "--force"])
            .assert()
            .success();
    }
    let response = capped_start(cfg, &repo, "collision");
    let args = pointer_args(&context_body(&response).expect("startup context"));
    assert_eq!(args, ["--store", "team", "curated:rules/big.md"]);
    let out = cargo_bin_cmd!("hm")
        .args(["--config", cfg, "--as-agent", "claude", "get"])
        .args(&args)
        .output()
        .expect("hm get");
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("team rule line 199"), "{text}");
    assert!(!text.contains("personal rule"));
}

#[test]
fn hm_get_reads_the_local_index_when_the_store_is_offline() {
    let fx = Fixture::new("delivery-get-offline");
    let output = cargo_bin_cmd!("hm")
        .args([
            "--config",
            fx.config_str(),
            "--as-agent",
            "claude",
            "remember",
            "--text",
            "Offline orbit fact.",
            "--json",
        ])
        .output()
        .expect("remember");
    let id = serde_json::from_slice::<Value>(&output.stdout).expect("json")["id"]
        .as_str()
        .expect("id")
        .to_owned();
    fx.refresh();
    let store = fx.config.parent().expect("dir").join("personal");
    fs::rename(&store, store.with_file_name("personal-offline")).expect("take store offline");
    let (code, text) = get(&fx, "claude", &[&id]);
    assert_eq!(code, Some(0), "{text}");
    assert!(text.contains("Offline orbit fact."));
}

#[test]
fn too_large_is_judged_against_the_whole_cap_not_the_space_after_recall() {
    let fx = Fixture::new("delivery-alone-max");
    let medium = format!("Medium startup note {}", "words ".repeat(90));
    fx.remember(&medium, None);
    fx.refresh();
    let full = fx.session_start("alone-full", &fx.repo_a, PERSISTENT);
    let cap = (context_body(&full).expect("body").len() + 40).to_string();
    let capped: &[&str] = &[
        "--delivery",
        "persistent",
        "--max-context-bytes",
        cap.as_str(),
    ];
    fx.session_start("alone", &fx.repo_a, capped);
    fx.hook("alone", &["context-reset"]);

    // Recall (project B's memory) takes space first, so the medium note no
    // longer fits this response, though it fits the whole cap.
    let first = fx.prompt("alone", &fx.repo_a, "zephyrine release checklist", capped);
    let body = context_body(&first).expect("recall plus context");
    assert!(body.contains(PROJECT_B));
    assert!(!body.contains("omitted=\"too-large\""), "{body}");
    assert!(!body.contains("Medium startup note"));
    // It is sent whole on the next prompt instead of becoming a pointer.
    let next = fx.prompt("alone", &fx.repo_a, "Inspect the tests.", capped);
    assert!(
        context_body(&next)
            .expect("drain")
            .contains("Medium startup note")
    );
}

#[test]
fn recall_and_context_share_one_delivery_without_duplicates() {
    let fx = Fixture::new("delivery-merge");
    fx.session_start("merge", &fx.repo_a, PERSISTENT);
    fx.hook("merge", &["context-reset"]);
    // After the reset the context selection is re-sent; the recall query also
    // hits a global memory the selection carries.
    let response = fx.prompt("merge", &fx.repo_a, "alpha", PERSISTENT);
    let body = context_body(&response).expect("combined block");
    assert_eq!(response["actions"].as_array().expect("actions").len(), 1);
    assert_eq!(body.matches(GLOBAL_A).count(), 1, "{body}");
    let state = fx.state_json("merge");
    let delivered = delivered_ids(&state);
    for id in emitted_ids(&response) {
        assert!(delivered.contains(&id), "{id} not delivered");
    }
    // Both halves are in the update: recall's memory and the selection's.
    assert!(emitted_ids(&response).len() >= 2, "{response}");
    assert!(state.get("delivered_context_key").is_some());
    assert!(state.get("delivered_recall_key").is_some());
}

#[test]
fn hm_get_into_a_closed_pipe_is_not_an_error() {
    let fx = Fixture::new("delivery-get-pipe");
    // Larger than a pipe buffer (64 KiB), so the write fails once the reader
    // is gone; still under the 128 KiB limit for one argument.
    let big = format!("Pipe orbit note {}", "padding ".repeat(12_000));
    let output = cargo_bin_cmd!("hm")
        .args([
            "--config",
            fx.config_str(),
            "--as-agent",
            "claude",
            "remember",
            "--text",
            &big,
            "--json",
        ])
        .output()
        .expect("remember");
    let id = serde_json::from_slice::<Value>(&output.stdout).expect("json")["id"]
        .as_str()
        .expect("id")
        .to_owned();
    fx.refresh();
    // A plain process, so the test controls when the reader goes away.
    let mut child = common::hermetic_hm_process()
        .args([
            "--config",
            fx.config_str(),
            "--as-agent",
            "claude",
            "get",
            &id,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn hm get");
    drop(child.stdout.take());
    let output = child.wait_with_output().expect("wait");
    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn recall_of_only_an_oversized_hit_emits_its_pointer() {
    let fx = Fixture::new("delivery-recall-pointer");
    fx.session_start("only-pointer", &fx.repo_a, PERSISTENT);
    let huge = format!("Pulsar calibration procedure {}", "padding ".repeat(400));
    fx.remember(&huge, None);
    fx.refresh();
    let capped: &[&str] = &["--delivery", "persistent", "--max-context-bytes", "1500"];
    let recall = fx.prompt("only-pointer", &fx.repo_a, "pulsar calibration", capped);
    let body = context_body(&recall).expect("pointer-only recall");
    assert!(body.contains("omitted=\"too-large\""));
    assert_eq!(recall["delivery"]["omitted"], 1);
    assert!(!body.contains("padding padding"));
}

#[test]
fn json_size_limit_counts_escaped_bytes() {
    let fx = Fixture::new("delivery-json-size");
    fx.remember(&format!("Quoted {}", "\"q\" ".repeat(150)), None);
    fx.refresh();
    // Warm the rendered cache first so every later start renders the same
    // freshness note and therefore the same output.
    fx.session_start("json-warm", &fx.repo_a, PERSISTENT);
    let full = fx.session_start("json-full", &fx.repo_a, PERSISTENT);
    let raw = context_body(&full).expect("body");
    let escaped = serde_json::to_string(&raw).expect("json").len() - 2;
    // Room for the whole block as raw bytes is not room once escaped.
    let raw_cap = raw.len().to_string();
    let json = fx.session_start(
        "json",
        &fx.repo_a,
        &[
            "--delivery",
            "persistent",
            "--max-context-bytes",
            &raw_cap,
            "--context-size",
            "json",
        ],
    );
    assert!(
        json["delivery"]["skipped_over_budget"]
            .as_u64()
            .expect("count")
            > 0
    );
    let exact = escaped.to_string();
    let fits = fx.session_start(
        "json-exact",
        &fx.repo_a,
        &[
            "--delivery",
            "persistent",
            "--max-context-bytes",
            &exact,
            "--context-size",
            "json",
        ],
    );
    assert_eq!(fits["delivery"]["skipped_over_budget"], 0);
    assert_eq!(emitted_ids(&fits).len(), emitted_ids(&full).len());
}
