//! Automatic recall must follow the user's subject without promoting incidental
//! framing words or carrying a subject across a context boundary.

use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

mod common;

const MODES: [(&str, &[&str]); 3] = [
    ("untracked", &[]),
    ("persistent", &["--delivery", "persistent"]),
    ("turn", &["--delivery", "turn"]),
];

struct Fixture {
    config: PathBuf,
    state: PathBuf,
    personal: PathBuf,
    repo_a: PathBuf,
    repo_b: PathBuf,
}

impl Fixture {
    /// Build real stores and project markers; references remain search-only.
    fn new(name: &str, backend: &str) -> Self {
        let root = common::temp_dir(name);
        let config = root.join("config.toml");
        let state = root.join("state");
        let personal = root.join("personal");
        let work = root.join("work");
        let repo_a = root.join("repo-a");
        let repo_b = root.join("repo-b");
        for (repo, id) in [(&repo_a, "recall-alpha"), (&repo_b, "recall-bravo")] {
            fs::create_dir_all(repo).expect("project directory");
            fs::write(
                repo.join(".hive-memory-project"),
                format!("id = \"{id}\"\n"),
            )
            .expect("project marker");
        }
        fs::write(
            &config,
            format!(
                r#"default_store = "personal"
state_dir = "{}"
cache_dir = "{}"
[stores.personal]
root = "{}"
[stores.work]
root = "{}"
[defaults]
context_strategy = "relevance"
search_backend = "{backend}"
search_scopes = ["global", "project", "agent-private"]
search_sources = ["remembered"]
[agents.claude]
read_stores = ["personal", "work"]
write_stores = ["personal", "work"]
"#,
                state.display(),
                root.join("cache").display(),
                personal.display(),
                work.display(),
            ),
        )
        .expect("configuration");
        for (name, store) in [("personal", &personal), ("work", &work)] {
            common::hermetic_hm()
                .args(["stores", "init", name, "--root", store.to_str().unwrap()])
                .assert()
                .success();
        }
        Self {
            config,
            state,
            personal,
            repo_a,
            repo_b,
        }
    }

    /// Execute the public CLI against this fixture, preserving JSON failures.
    fn run(&self, store: &str, session: Option<&str>, args: &[&str]) -> Value {
        let mut cmd = common::hermetic_hm();
        cmd.args([
            "--config",
            self.config.to_str().unwrap(),
            "--store",
            store,
            "--as-agent",
            "claude",
        ]);
        if let Some(session) = session {
            cmd.env("HIVE_MEMORY_SESSION_ID", session);
        }
        let output = cmd.args(args).arg("--json").output().expect("run hm");
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("JSON output")
    }

    fn remember(&self, store: &str, text: &str, extra: &[&str]) -> String {
        let mut args = vec!["remember", "--kind", "reference", "--text", text];
        args.extend_from_slice(extra);
        self.run(store, None, &args)["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn refresh(&self) {
        for store in ["personal", "work"] {
            self.run(store, None, &["refresh", "--force"]);
        }
    }

    fn prompt(&self, session: &str, store: &str, repo: &Path, text: &str, mode: &[&str]) -> Value {
        let mut args = vec![
            "hook",
            "prompt-submit",
            "--project",
            repo.to_str().unwrap(),
            "--text",
            text,
        ];
        args.extend_from_slice(mode);
        self.run(store, Some(session), &args)
    }

    /// Spend the context-selection change before observing incremental recall.
    fn prime(&self, session: &str, mode: &[&str]) {
        self.prompt(
            session,
            "personal",
            &self.repo_a,
            "Inspect the tests.",
            mode,
        );
    }

    fn topic(&self, store: &str, subject: &str, count: usize) -> BTreeSet<String> {
        (0..count)
            .map(|n| {
                self.remember(
                    store,
                    &format!("{subject} diagnostic workflow reference number {n}."),
                    &[],
                )
            })
            .collect()
    }
}

/// Use structured selections, so display wording cannot make a test pass.
fn selected(response: &Value) -> BTreeSet<String> {
    response["recall"]["selected_ids"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|id| id.as_str().expect("selected id").to_owned())
        .collect()
}

/// Read memory IDs from the public context wire format. Legacy untracked
/// responses have no structured delivery report for normal-context backfill.
fn injected(response: &Value) -> BTreeSet<String> {
    response["actions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|action| action["kind"] == "inject_context")
        .flat_map(|action| {
            action["body"]
                .as_str()
                .expect("context body")
                .split("<memory id=\"")
                .skip(1)
                .map(|block| block.split('"').next().expect("memory id").to_owned())
        })
        .collect()
}

#[test]
fn prompt_backfill_does_not_drain_unrelated_startup_memories() {
    let fx = Fixture::new("prompt-backfill", "lexical");
    let config = fs::read_to_string(&fx.config).expect("config");
    fs::write(
        &fx.config,
        config.replace(
            "context_strategy = \"relevance\"",
            "context_strategy = \"recency\"",
        ),
    )
    .expect("recency configuration");
    let relevant = fx.topic("personal", "AgentGuard telemetry", 4);
    let unrelated = fx.topic("personal", "Orchard watering garden schedule", 12);
    fx.refresh();
    let full = fx.run(
        "personal",
        Some("backfill-full"),
        &[
            "hook",
            "session-start",
            "--project",
            fx.repo_a.to_str().unwrap(),
            "--delivery",
            "persistent",
        ],
    );
    let all = injected(&full);
    assert!(
        relevant.is_subset(&all) && unrelated.is_subset(&all),
        "startup selection must retain its existing broad semantics: {full}"
    );
    // A real host-sized byte cap leaves both unrelated startup notes and some
    // same-topic notes pending after the first explicit recall.
    let cap = "900";

    for (name, mode) in [MODES[1], MODES[2], MODES[0]] {
        let mut startup = vec![
            "hook",
            "session-start",
            "--project",
            fx.repo_a.to_str().unwrap(),
            "--max-context-bytes",
            cap,
        ];
        // Muse's startup context persists even when its later prompt contexts
        // have turn lifetime; untracked hosts keep their existing legacy path.
        if name != "untracked" {
            startup.extend_from_slice(&["--delivery", "persistent"]);
        }
        let first = fx.run("personal", Some(name), &startup);
        let initial = injected(&first);
        assert!(
            !initial.is_disjoint(&unrelated),
            "{name}: startup control: {first}"
        );
        assert!(
            !unrelated.is_subset(&initial),
            "{name}: fixture needs pending startup notes"
        );
        let mut flags = mode.to_vec();
        flags.extend_from_slice(&["--max-context-bytes", cap]);
        let explicit = fx.prompt(name, "personal", &fx.repo_a, "AgentGuard telemetry", &flags);
        let ids = injected(&explicit);
        assert!(
            !ids.is_disjoint(&relevant),
            "{name}: useful prompt context lost: {explicit}"
        );
        assert!(
            ids.is_disjoint(&unrelated),
            "{name}: explicit prompt drained unrelated startup notes: {explicit}"
        );
        assert!(
            !relevant.is_subset(&ids),
            "{name}: fixture needs a same-topic tail"
        );
        for continuation in ["explain #5", "do those three"] {
            let response = fx.prompt(name, "personal", &fx.repo_a, continuation, &flags);
            assert!(
                injected(&response).is_disjoint(&unrelated),
                "{name}: generic continuation drained unrelated startup notes: {response}"
            );
            if name != "turn" {
                assert!(
                    injected(&response).is_empty(),
                    "{name}: generic continuation drained the visible topic's tail: {response}"
                );
            }
        }
    }
}

#[test]
fn late_explicit_subject_survives_conversational_framing() {
    let fx = Fixture::new("late-subject", "tantivy");
    let relevant = fx.remember(
        "personal",
        "AgentGuard telemetry preserves hook command attribution.",
        &[],
    );
    let distractors = fx.topic("personal", "Another unrelated garden planting", 4);
    fx.refresh();
    for (name, mode) in MODES {
        fx.prime(name, mode);
        let response = fx.prompt(
            name,
            "personal",
            &fx.repo_a,
            "can you do another AgentGuard telemetry scrub?",
            mode,
        );
        let ids = selected(&response);
        assert!(ids.contains(&relevant), "{name}: {response}");
        assert!(
            ids.is_disjoint(&distractors),
            "{name}: framing recalled unrelated notes: {response}"
        );
    }
}

#[test]
fn known_strong_hit_cannot_qualify_unrelated_unseen_results() {
    let fx = Fixture::new("known-strong", "tantivy");
    let strong = fx.remember(
        "personal",
        "AgentGuard telemetry preserves command attribution for hook diagnostics.",
        &[],
    );
    fx.refresh();
    for (name, mode) in MODES {
        fx.prime(name, mode);
        let first = fx.prompt(name, "personal", &fx.repo_a, "AgentGuard telemetry", mode);
        assert_eq!(
            selected(&first),
            BTreeSet::from([strong.clone()]),
            "{name}: {first}"
        );
    }
    let unrelated = fx.topic("personal", "Telemetry weather station rain gauge", 4);
    fx.refresh();
    for (name, mode) in MODES {
        let response = fx.prompt(name, "personal", &fx.repo_a, "AgentGuard telemetry", mode);
        assert!(
            selected(&response).is_disjoint(&unrelated),
            "{name}: {response}"
        );
    }
}

#[test]
fn generic_followups_reuse_the_subject_without_draining_visible_topics() {
    let fx = Fixture::new("followups", "lexical");
    let related = fx.topic("personal", "AgentGuard telemetry", 4);
    fx.refresh();
    for (name, mode) in MODES {
        for (n, followup) in [
            "explain #5",
            "do those three",
            "any unexpected side effects?",
            "create a PR for the change",
            "and merge when ready",
        ]
        .iter()
        .enumerate()
        {
            let session = format!("{name}-{n}");
            fx.prime(&session, mode);
            let first = fx.prompt(
                &session,
                "personal",
                &fx.repo_a,
                "AgentGuard telemetry",
                mode,
            );
            let unseen: BTreeSet<_> = related.difference(&selected(&first)).cloned().collect();
            assert!(
                !unseen.is_empty(),
                "fixture must leave a relevant result for the followup"
            );
            let response = fx.prompt(&session, "personal", &fx.repo_a, followup, mode);
            if name == "turn" {
                // Turn recall is gone from view; unseen strong matches remain
                // useful when the continuation still names that subject.
                assert!(
                    !selected(&response).is_disjoint(&unseen),
                    "{name} {followup}: {response}"
                );
            } else {
                // A vague continuation must not walk the unseen tail when a
                // best-grade answer is already visible in persistent context.
                assert!(
                    selected(&response).is_empty(),
                    "{name} {followup}: {response}"
                );
            }
            assert!(
                selected(&response).is_subset(&related),
                "{name}: {response}"
            );
        }
    }
}

#[test]
fn conversational_framing_keeps_the_interface_subject_ahead_of_ambient_hosts() {
    let fx = Fixture::new("interfaces-framing", "tantivy");
    fs::write(fx.personal.join("entities.toml"),
        "schema_version = 1\n[[entity]]\nid = \"host:maple\"\naliases = [\"Maple\"]\n[[entity]]\nid = \"topic:interfaces\"\naliases = [\"network interface\", \"network interfaces\"]\n"
    ).expect("synthetic host and topic aliases");
    let relevant = fx.remember("personal",
        "Disabled network interface inventory on Cedar documents intentional virtual bridges and tunnels.", &[]);
    let ambient = fx.topic("personal", "Maple backup rotation archive schedule", 4);
    fx.refresh();
    fx.prime("interfaces", &[]);
    let response = fx.prompt("interfaces", "personal", &fx.repo_a,
        "I see lots of disabled network interfaces on Maple. Should those be cleaned up? Do other hosts have this also?", &[]);
    let ids = selected(&response);
    assert!(
        ids.contains(&relevant),
        "cross-host interface guidance was lost: {response}"
    );
    assert!(
        ids.is_disjoint(&ambient),
        "hostname alone replaced the interface subject: {response}"
    );
}

#[test]
fn workflow_continuations_do_not_erase_pr_watch_or_domain_negation() {
    let fx = Fixture::new("domain-intent", "lexical");
    let watch = fx.remember(
        "personal",
        "PR watch lifecycle preserves monitoring obligations until checks settle.",
        &[],
    );
    let negative = fx.remember(
        "personal",
        "Git push is not allowed during release deployment.",
        &[],
    );
    let positive = fx.remember(
        "personal",
        "Git push is allowed during release deployment.",
        &[],
    );
    fx.refresh();
    fx.prime("watch", &[]);
    let response = fx.prompt("watch", "personal", &fx.repo_a, "inspect PR watch", &[]);
    assert!(
        selected(&response).contains(&watch),
        "PR watch is a domain subject: {response}"
    );
    fx.prime("negation", &[]);
    let response = fx.prompt(
        "negation",
        "personal",
        &fx.repo_a,
        "git push not allowed",
        &[],
    );
    let ids = selected(&response);
    assert!(
        ids.contains(&negative),
        "domain negation was lost: {response}"
    );
    assert!(
        !ids.contains(&positive),
        "opposite instruction was recalled: {response}"
    );
}

#[test]
fn explicit_new_subject_replaces_the_previous_followup_topic() {
    let fx = Fixture::new("new-subject", "lexical");
    let old = fx.topic("personal", "AgentGuard telemetry", 4);
    let current = fx.topic("personal", "Zircon storage", 4);
    fx.refresh();
    let mode = &["--delivery", "turn"];
    fx.prime("switch", mode);
    fx.prompt(
        "switch",
        "personal",
        &fx.repo_a,
        "AgentGuard telemetry",
        mode,
    );
    let explicit = fx.prompt("switch", "personal", &fx.repo_a, "Zircon storage", mode);
    let unseen: BTreeSet<_> = current.difference(&selected(&explicit)).cloned().collect();
    assert!(!selected(&explicit).is_empty(), "{explicit}");
    let followup = fx.prompt("switch", "personal", &fx.repo_a, "explain #5", mode);
    assert!(!selected(&followup).is_disjoint(&unseen), "{followup}");
    assert!(selected(&followup).is_disjoint(&old), "{followup}");
}

#[test]
fn short_explicit_subject_replaces_the_previous_topic() {
    let fx = Fixture::new("short-subject", "lexical");
    let previous = fx.topic("personal", "AgentGuard telemetry", 4);
    let current = fx.topic("personal", "GPU cooling", 4);
    fx.refresh();
    let mode = &["--delivery", "persistent"];
    fx.prime("short-subject", mode);
    fx.prompt(
        "short-subject",
        "personal",
        &fx.repo_a,
        "AgentGuard telemetry",
        mode,
    );
    // A short acronym is an explicit subject, not an anaphoric continuation.
    let response = fx.prompt("short-subject", "personal", &fx.repo_a, "check GPU", mode);
    let ids = selected(&response);
    assert!(
        !ids.is_disjoint(&current),
        "short explicit topic was discarded: {response}"
    );
    assert!(
        ids.is_disjoint(&previous),
        "old subject leaked into explicit new topic: {response}"
    );
}

#[test]
fn unrepresentable_explicit_subject_clears_the_previous_topic() {
    let fx = Fixture::new("oversized-subject", "lexical");
    let previous = fx.topic("personal", "AgentGuard telemetry", 4);
    fx.refresh();
    let mode = &["--delivery", "persistent"];
    fx.prime("oversized-subject", mode);
    fx.prompt(
        "oversized-subject",
        "personal",
        &fx.repo_a,
        "AgentGuard telemetry",
        mode,
    );
    // An explicit identifier outside the bounded sketch is not a continuation
    // of the previous task. Skipping recall is safer than injecting that task.
    let oversized = format!("inspect {}", "x".repeat(257));
    for prompt in [oversized.as_str(), "explain #5"] {
        let response = fx.prompt("oversized-subject", "personal", &fx.repo_a, prompt, mode);
        assert!(
            selected(&response).is_disjoint(&previous),
            "unrepresentable new topic inherited the prior task: {response}"
        );
    }
}

#[test]
fn context_boundaries_prevent_inheriting_a_stale_subject() {
    for boundary in [
        "missing-state",
        "context-reset",
        "project-switch",
        "store-switch",
    ] {
        let fx = Fixture::new(boundary, "lexical");
        let mut old = fx.topic("personal", "AgentGuard telemetry", 4);
        old.extend(fx.topic("work", "AgentGuard telemetry", 4));
        fx.refresh();
        fx.prime(boundary, &[]);
        fx.prompt(
            boundary,
            "personal",
            &fx.repo_a,
            "AgentGuard telemetry",
            &[],
        );
        match boundary {
            "missing-state" => {
                fs::remove_file(fx.state.join(format!("runs/{boundary}/hook-state.json")))
                    .expect("remove old session state")
            }
            "context-reset" => {
                fx.run("personal", Some(boundary), &["hook", "context-reset"]);
            }
            _ => {}
        }
        let store = if boundary == "store-switch" {
            "work"
        } else {
            "personal"
        };
        let repo = if boundary == "project-switch" {
            &fx.repo_b
        } else {
            &fx.repo_a
        };
        // A boundary may re-emit the empty startup selection on the first
        // prompt; the second checks that it did not retain a stale topic.
        for _ in 0..2 {
            let response = fx.prompt(boundary, store, repo, "explain #5", &[]);
            assert!(
                selected(&response).is_disjoint(&old),
                "{boundary}: {response}"
            );
        }
    }
}

#[test]
fn no_hit_and_all_known_prompts_still_establish_a_bounded_followup_topic() {
    for phase in ["no-hit", "all-known"] {
        let fx = Fixture::new(phase, "lexical");
        if phase == "all-known" {
            fx.topic("personal", "AgentGuard telemetry", 1);
        }
        fx.refresh();
        fx.prime(phase, &["--delivery", "persistent"]);
        let prompt = "Please review AgentGuard telemetry while keeping this conversational framing out of durable recall state.";
        fx.prompt(
            phase,
            "personal",
            &fx.repo_a,
            prompt,
            &["--delivery", "persistent"],
        );
        if phase == "all-known" {
            fx.prompt(
                phase,
                "personal",
                &fx.repo_a,
                prompt,
                &["--delivery", "persistent"],
            );
        }
        let state = fs::read_to_string(fx.state.join(format!("runs/{phase}/hook-state.json")))
            .unwrap_or_default();
        assert!(
            !state.contains(prompt),
            "a recall topic must not persist the raw user message"
        );
        let added = fx.remember(
            "personal",
            "AgentGuard telemetry diagnostic workflow newly indexed reference.",
            &[],
        );
        fx.refresh();
        let followup = fx.prompt(
            phase,
            "personal",
            &fx.repo_a,
            "do those three",
            &["--delivery", "persistent"],
        );
        if phase == "no-hit" {
            assert!(selected(&followup).contains(&added), "{phase}: {followup}");
        } else {
            assert!(selected(&followup).is_empty(), "{phase}: {followup}");
            // Explicitly asking about the subject still discovers a new note;
            // continuation abstention must not make it permanently invisible.
            let explicit = fx.prompt(
                phase,
                "personal",
                &fx.repo_a,
                "AgentGuard telemetry",
                &["--delivery", "persistent"],
            );
            assert!(selected(&explicit).contains(&added), "{explicit}");
        }
    }
}

#[test]
fn entity_aliases_and_bm25_paraphrases_keep_useful_recall() {
    let fx = Fixture::new("aliases-paraphrases", "tantivy");
    fs::write(fx.personal.join("entities.toml"), "schema_version = 1\n[[entity]]\nid = \"runbook:deploy\"\naliases = [\"deployment launch\", \"apogee runbook\"]\n").expect("entity aliases");
    let alias = fx.remember(
        "personal",
        "The deployment launch procedure requires a signed manifest.",
        &[],
    );
    let paraphrase = fx.remember(
        "personal",
        "The alpha service uses a manual verification checklist.",
        &[],
    );
    fx.refresh();
    for (session, prompt, id) in [
        ("alias", "Where is the apogee runbook?", alias),
        ("bm25", "alpha pipeline", paraphrase),
    ] {
        fx.prime(session, &[]);
        let response = fx.prompt(session, "personal", &fx.repo_a, prompt, &[]);
        assert!(selected(&response).contains(&id), "{session}: {response}");
    }
}

#[test]
fn stronger_recall_keeps_mandatory_visibility_and_correction_filters() {
    for filter in ["audience", "scope", "source", "validity", "supersession"] {
        let fx = Fixture::new(filter, "tantivy");
        if filter == "scope" {
            // Search is intentionally broad across projects, but a configured
            // scope exclusion must still apply to automatic recall.
            let config = fs::read_to_string(&fx.config).expect("config");
            fs::write(
                &fx.config,
                config.replace(
                    "search_scopes = [\"global\", \"project\", \"agent-private\"]",
                    "search_scopes = [\"global\", \"agent-private\"]",
                ),
            )
            .expect("narrow search scopes");
        }
        let control = fx.remember(
            "personal",
            "AgentGuard telemetry diagnostic workflow uses structured attribution.",
            &[],
        );
        let text = "AgentGuard telemetry diagnostic workflow uses a retired approach.";
        let hidden = match filter {
            "audience" => fx.remember(
                "personal",
                text,
                &["--scope", "agent-private", "--audience", "muse"],
            ),
            "scope" => fx.remember(
                "personal",
                text,
                &[
                    "--scope",
                    "project",
                    "--project",
                    fx.repo_b.to_str().unwrap(),
                ],
            ),
            "source" => fx.run(
                "personal",
                None,
                &["note", "--kind", "reference", "--text", text],
            )["id"]
                .as_str()
                .unwrap()
                .to_owned(),
            "validity" => fx.remember("personal", text, &["--valid-to", "2000-01-01T00:00:00Z"]),
            "supersession" => {
                let old = fx.remember("personal", text, &[]);
                fx.remember(
                    "personal",
                    "A corrected workflow requires the current attribution implementation.",
                    &["--supersedes", &old],
                );
                old
            }
            _ => unreachable!(),
        };
        fx.refresh();
        fx.prime(filter, &[]);
        let response = fx.prompt(filter, "personal", &fx.repo_a, "AgentGuard telemetry", &[]);
        let ids = selected(&response);
        assert!(
            ids.contains(&control),
            "{filter}: positive control lost: {response}"
        );
        assert!(
            !ids.contains(&hidden),
            "{filter}: forbidden result returned: {response}"
        );
    }
}

#[test]
fn another_agents_lexical_match_cannot_suppress_permitted_alias_backfill() {
    let fx = Fixture::new("private-evidence-bar", "tantivy");
    let config = fs::read_to_string(&fx.config).expect("config");
    fs::write(
        &fx.config,
        config.replace(
            "context_strategy = \"relevance\"",
            "context_strategy = \"recency\"",
        ),
    )
    .expect("recency configuration");
    fs::write(fx.personal.join("entities.toml"),
        "schema_version = 1\n[[entity]]\nid = \"runbook:deploy\"\naliases = [\"deployment launch\", \"apogee runbook\"]\n"
    ).expect("entity aliases");
    let public: BTreeSet<_> = (0..4)
        .map(|n| {
            fx.remember(
                "personal",
                &format!("Deployment launch runbook requires signed manifests for stage {n}."),
                &[],
            )
        })
        .collect();
    fx.refresh();
    let mode = &["--delivery", "persistent"];
    for session in ["before-private", "after-private"] {
        if session == "after-private" {
            fx.remember(
                "personal",
                "Apogee runbook requires another agent's private procedure.",
                &["--scope", "agent-private", "--audience", "muse"],
            );
            fx.refresh();
        }
        // Four relevant notes exceed recall's three-hit cap, so the fourth
        // proves that ordinary-context backfill also respects audience policy
        // when deciding which relevance tier should win.
        let start = fx.run(
            "personal",
            Some(session),
            &[
                "hook",
                "session-start",
                "--project",
                fx.repo_a.to_str().unwrap(),
                "--delivery",
                "persistent",
                "--max-context-bytes",
                "10",
            ],
        );
        assert!(
            injected(&start).is_empty(),
            "startup cap must leave a backlog"
        );
        let response = fx.prompt(session, "personal", &fx.repo_a, "apogee runbook", mode);
        assert_eq!(
            injected(&response),
            public,
            "{session}: ineligible evidence changed permitted alias context: {response}"
        );
    }
}

#[test]
fn credential_bearing_prompts_clear_topics_without_persisting_values() {
    let fx = Fixture::new("private-prompt-topic", "lexical");
    fx.refresh();
    // Construct detector fixtures so source scans cannot mistake these public
    // tests for actual credentials. None is a credential for a real service.
    let cases = [
        ("aws", ["AK", "IA", &"Q".repeat(16)].concat()),
        ("github", ["gh", "p_", &"a".repeat(36)].concat()),
        (
            "assignment",
            ["local", "credential", "value", "for", "unit", "checks"].concat(),
        ),
    ];
    for (mode_name, mode) in MODES {
        for (kind, value) in &cases {
            let session = format!("credential-{mode_name}-{kind}");
            fx.prompt(
                &session,
                "personal",
                &fx.repo_a,
                "AgentGuard telemetry",
                mode,
            );
            let path = fx.state.join(format!("runs/{session}/hook-state.json"));
            let before: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            assert!(before["recall_topic"].is_object(), "establish prior topic");
            let text = if *kind == "assignment" {
                format!("Inspect api_key: {value}")
            } else {
                format!("Inspect {value}")
            };
            assert!(
                !hive_memory::secret::detect(&text).is_empty(),
                "detector fixture"
            );
            let mut cmd = common::hermetic_hm();
            let output = cmd
                .env("HIVE_MEMORY_SESSION_ID", &session)
                .args([
                    "--config",
                    fx.config.to_str().unwrap(),
                    "--store",
                    "personal",
                    "--as-agent",
                    "claude",
                    "hook",
                    "prompt-submit",
                    "--project",
                    fx.repo_a.to_str().unwrap(),
                    "--text",
                    &text,
                    "--json",
                ])
                .args(mode)
                .output()
                .expect("run credential-bearing prompt");
            // Failure messages deliberately identify the case, never its value.
            assert!(output.status.success(), "{mode_name}/{kind}: hook failed");
            let response: Value = serde_json::from_slice(&output.stdout).expect("hook JSON");
            assert!(
                injected(&response).is_empty(),
                "{mode_name}/{kind}: no injection"
            );
            let state_text = fs::read_to_string(&path).expect("topic state");
            let state: Value = serde_json::from_str(&state_text).expect("state JSON");
            assert!(
                state["recall_topic"].is_null(),
                "{mode_name}/{kind}: clear prior topic"
            );
            for forbidden in [&text, value, &value.to_ascii_lowercase()] {
                assert!(
                    !state_text.contains(forbidden),
                    "{mode_name}/{kind}: private value persisted"
                );
                assert!(
                    !String::from_utf8_lossy(&output.stdout).contains(forbidden),
                    "{mode_name}/{kind}: private value echoed"
                );
                assert!(
                    !String::from_utf8_lossy(&output.stderr).contains(forbidden),
                    "{mode_name}/{kind}: private value logged"
                );
            }
            fx.prompt(&session, "personal", &fx.repo_a, "Explain #5", mode);
            let next: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
            assert!(
                next["recall_topic"].is_null(),
                "{mode_name}/{kind}: no stale inheritance"
            );
        }
    }
}

#[test]
fn stored_credential_topics_are_rejected_before_generic_reuse() {
    let fx = Fixture::new("private-stored-topic", "lexical");
    fx.refresh();
    let token = ["gh", "p_", &"b".repeat(36)].concat();
    for (name, mode) in MODES {
        let session = format!("stored-credential-{name}");
        fx.prompt(
            &session,
            "personal",
            &fx.repo_a,
            "AgentGuard telemetry",
            mode,
        );
        let path = fx.state.join(format!("runs/{session}/hook-state.json"));
        let mut state: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        // Model a legacy or corrupted local state file; keep its valid binding
        // so rejection must happen at the topic's read boundary.
        state["recall_topic"]["query"] = Value::String(token.clone());
        fs::write(&path, serde_json::to_vec(&state).unwrap()).expect("seed isolated state");
        let response = fx.prompt(&session, "personal", &fx.repo_a, "Explain #5", mode);
        assert!(
            injected(&response).is_empty(),
            "{name}: no private injection"
        );
        assert!(
            !response.to_string().contains(&token),
            "{name}: no value echo"
        );
        let state_text = fs::read_to_string(path).expect("updated state");
        let state: Value = serde_json::from_str(&state_text).expect("state JSON");
        assert!(
            state["recall_topic"].is_null(),
            "{name}: reject stored credential topic"
        );
        assert!(!state_text.contains(&token), "{name}: clear stored value");
    }
}
