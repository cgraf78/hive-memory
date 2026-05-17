use assert_cmd::cargo::cargo_bin_cmd;
use hive_memory::config::Sensitivity;
use hive_memory::store::{self, StoreInitOptions};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

// These are intentionally filesystem-level simulations instead of tests for a
// specific cloud vendor. The v1 contract is that independent immutable writes
// merge, suspicious conflict copies are quarantined for manual recovery, and
// ordinary rename propagation can be reindexed without losing searchability.

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "hive-memory-cloud-sync-{name}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("create temp dir");
    path
}

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

fn hm<const N: usize>(config: &Path, args: [&str; N]) {
    cargo_bin_cmd!("hm")
        .arg("--config")
        .arg(config)
        .args(args)
        .assert()
        .success();
}

fn hm_stdout<const N: usize>(config: &Path, args: [&str; N]) -> String {
    let output = cargo_bin_cmd!("hm")
        .arg("--config")
        .arg(config)
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
