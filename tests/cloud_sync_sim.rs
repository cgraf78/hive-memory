use hive_memory::config::Sensitivity;
use hive_memory::store::{self, StoreInitOptions};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

mod common;
use common::temp_dir;

// These are intentionally filesystem-level simulations instead of tests for a
// specific cloud vendor. The v1 contract is that independent immutable writes
// merge, suspicious conflict copies are quarantined for manual recovery, and
// ordinary rename propagation can be reindexed without losing searchability.

#[test]
#[ignore = "CI runs the cloud-sync simulation explicitly"]
fn independent_writes_survive_directory_merge() {
    let dir = temp_dir("merge");
    let host_a = dir.join("host-a");
    let host_b = dir.join("host-b");
    init_store(&host_a);
    copy_tree(&host_a, &host_b);
    let config_a = write_config(&dir.join("a-config.toml"), &host_a);
    let config_b = write_config(&dir.join("b-config.toml"), &host_b);

    hm(
        &config_a,
        ["remember", "--text", "cloud merge keeps host A memory"],
    );
    hm(
        &config_b,
        ["remember", "--text", "cloud merge keeps host B memory"],
    );

    copy_tree(&host_b.join("inbox/notes"), &host_a.join("inbox/notes"));
    copy_tree(&host_b.join("inbox/events"), &host_a.join("inbox/events"));

    hm(&config_a, ["refresh", "--force", "--quiet"]);
    let search = hm_stdout(&config_a, ["search", "cloud merge", "--json"]);

    assert!(search.contains("cloud merge keeps host A memory"));
    assert!(search.contains("cloud merge keeps host B memory"));
}

#[test]
#[ignore = "CI runs the cloud-sync simulation explicitly"]
fn conflict_copies_are_quarantined_without_deleting_memory() {
    let dir = temp_dir("conflict");
    let root = dir.join("store");
    init_store(&root);
    let config = write_config(&dir.join("config.toml"), &root);
    let conflict_dir = root.join("inbox/notes/2026/05/17");
    fs::create_dir_all(&conflict_dir).expect("conflict dir");
    let conflict = conflict_dir.join("memory conflicted copy.md");
    fs::write(&conflict, "divergent cloud memory").expect("conflict file");

    let output = hm_stdout(&config, ["doctor", "--quick", "--fix", "--json"]);

    assert!(output.contains("\"fixed\": 1"));
    assert!(!conflict.exists());
    assert!(
        root.join(".quarantine/cloud-conflicts").exists(),
        "conflict file remains recoverable under quarantine"
    );
}

#[test]
#[ignore = "CI runs the cloud-sync simulation explicitly"]
fn cloud_renamed_notes_are_reindexed() {
    let dir = temp_dir("rename");
    let root = dir.join("store");
    init_store(&root);
    let config = write_config(&dir.join("config.toml"), &root);
    hm(
        &config,
        ["remember", "--text", "cloud rename keeps searchable memory"],
    );
    let note = markdown_files(&root.join("inbox/notes"))
        .into_iter()
        .next()
        .expect("written note");
    let renamed = note.with_file_name("renamed-by-cloud.md");
    fs::rename(&note, &renamed).expect("rename note");

    hm(&config, ["refresh", "--force", "--quiet"]);
    let search = hm_stdout(&config, ["search", "cloud rename", "--json"]);

    assert!(search.contains("cloud rename keeps searchable memory"));
    assert!(search.contains("renamed-by-cloud.md"));
}

/// A retag on host A arrives at host B as an in-place rewrite carrying A's
/// mtime, older than B's own newest note, under date folders whose mtimes the
/// cloud never moves. B's warm read path (no `hm refresh`) must stop serving
/// the record at its old, wider scope.
#[test]
#[ignore = "CI runs the cloud-sync simulation explicitly"]
fn synced_retag_narrows_peer_search_without_refresh() {
    let dir = temp_dir("synced-retag");
    let host_a = dir.join("host-a");
    let host_b = dir.join("host-b");
    init_store(&host_a);
    // Separate config dirs give each host its own local cache, as on real
    // machines; only the store tree is shared through "sync".
    fs::create_dir_all(dir.join("a")).expect("host A config dir");
    fs::create_dir_all(dir.join("b")).expect("host B config dir");
    let config_a = write_config(&dir.join("a/config.toml"), &host_a);
    let config_b = write_config(&dir.join("b/config.toml"), &host_b);

    let remembered = hm_stdout(
        &config_a,
        [
            "remember",
            "--scope",
            "global",
            "--project-id",
            "repo-alpha",
            "--text",
            "synced retag narrows Cedar policy memory",
            "--json",
        ],
    );
    let remembered: serde_json::Value = serde_json::from_str(&remembered).expect("remember json");
    let id = remembered["id"].as_str().expect("memory id").to_owned();
    copy_tree(&host_a, &host_b);
    hm(&config_a, ["retag", &id, "--scope", "project"]);
    // B writes after A's retag but before sync delivers it, so B's own note is
    // the newest file and A's preserved retag mtime cannot move that aggregate.
    hm(
        &config_b,
        ["remember", "--text", "host B writes a newer note"],
    );
    let peer_search = ["search", "Cedar policy", "--scope", "global"];
    assert!(hm_stdout(&config_b, peer_search).contains("hits: 1"));

    sync_rewrites(&host_a, &host_b);

    assert!(
        hm_stdout(&config_b, peer_search).contains("hits: 0"),
        "host B must not keep serving the pre-retag global scope"
    );
}

/// Deliver files that exist on both hosts but differ, the way rclone/Drive
/// does: overwrite in place, keep the source mtime, leave folder mtimes alone.
fn sync_rewrites(source: &Path, destination: &Path) {
    for entry in fs::read_dir(source).expect("read sync source") {
        let entry = entry.expect("sync entry");
        let from = entry.path();
        let to = destination.join(entry.file_name());
        if entry.file_type().expect("sync file type").is_dir() {
            if to.is_dir() {
                sync_rewrites(&from, &to);
            }
            continue;
        }
        if !to.is_file() || fs::read(&from).ok() == fs::read(&to).ok() {
            continue;
        }
        let parent = to.parent().expect("sync parent");
        let parent_mtime = modified(parent);
        fs::write(&to, fs::read(&from).expect("read synced file")).expect("rewrite synced file");
        set_modified(&to, modified(&from));
        set_modified(parent, parent_mtime);
    }
}

fn modified(path: &Path) -> SystemTime {
    fs::metadata(path)
        .expect("metadata")
        .modified()
        .expect("mtime")
}

fn set_modified(path: &Path, mtime: SystemTime) {
    // Directories open read-only on Unix; futimens only needs ownership.
    fs::File::open(path)
        .or_else(|_| fs::File::options().write(true).open(path))
        .expect("open for set_times")
        .set_times(fs::FileTimes::new().set_modified(mtime))
        .expect("set mtime");
}

fn init_store(root: &Path) {
    store::init_store(&StoreInitOptions {
        name: "personal".to_owned(),
        root: root.to_path_buf(),
        description: Some("Cloud sync simulation store".to_owned()),
        sensitivity: Sensitivity::Private,
    })
    .expect("init store");
}

fn write_config(path: &Path, root: &Path) -> PathBuf {
    let parent = path.parent().expect("config parent");
    fs::write(
        path,
        format!(
            r#"
            default_store = "personal"
            data_dir = "{}"
            state_dir = "{}"
            cache_dir = "{}"

            [stores.personal]
            root = "{}"
            "#,
            parent.join("data").display(),
            parent.join("state").display(),
            parent.join("cache").display(),
            root.display()
        ),
    )
    .expect("write config");
    path.to_path_buf()
}

/// Build an `hm` invocation pinned to `config`, isolated from the caller's hm
/// environment by [`common::hermetic_hm`].
fn hm_cmd(config: &Path) -> assert_cmd::Command {
    let mut cmd = common::hermetic_hm();
    cmd.arg("--config").arg(config);
    cmd
}

fn hm<const N: usize>(config: &Path, args: [&str; N]) {
    hm_cmd(config).args(args).assert().success();
}

fn hm_stdout<const N: usize>(config: &Path, args: [&str; N]) -> String {
    let output = hm_cmd(config)
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(output).expect("utf8 stdout")
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).expect("copy destination");
    for entry in fs::read_dir(source).expect("read copy source") {
        let entry = entry.expect("copy entry");
        let from = entry.path();
        let to = destination.join(entry.file_name());
        let file_type = entry.file_type().expect("copy file type");
        if file_type.is_dir() {
            copy_tree(&from, &to);
        } else if !to.exists() {
            // Cloud drives generally converge by adding missing files. Do not
            // overwrite here; that would hide whether Hive Memory's unique file
            // naming is actually preventing write collisions.
            fs::copy(&from, &to).expect("copy file");
        }
    }
}

fn markdown_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_markdown(root, &mut files);
    files.sort();
    files
}

fn collect_markdown(root: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(root).expect("read markdown dir") {
        let entry = entry.expect("markdown entry");
        let path = entry.path();
        let file_type = entry.file_type().expect("markdown file type");
        if file_type.is_dir() {
            collect_markdown(&path, files);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
            files.push(path);
        }
    }
}
