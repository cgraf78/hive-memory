//! Scratch directories for unit tests.
//!
//! Directories created here live under one root per test thread. libtest runs
//! each test on its own thread and joins it before exiting, so the root, and
//! everything a test put in it, is deleted when the test ends, pass or fail,
//! without each test holding a guard.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

thread_local! {
    static SCRATCH_ROOT: tempfile::TempDir = tempfile::Builder::new()
        .prefix("hive-memory-unit-")
        .tempdir()
        .expect("create scratch root");
}

/// Create a new, empty directory labelled `name`, removed when the calling
/// test's thread exits.
///
/// Every call returns a distinct directory, even for a repeated `name`. A
/// process the test leaves running can still recreate part of one after the
/// test ends; the random names keep that harmless.
pub(crate) fn temp_dir(name: &str) -> PathBuf {
    SCRATCH_ROOT.with(|root| {
        tempfile::Builder::new()
            .prefix(&format!("{name}-"))
            // tempfile defaults to 0700. Keep the umask-style 0755 that the
            // old `create_dir_all` fixtures had, so store-permission checks see
            // an ordinary directory unless a test chmods it on purpose.
            .permissions(std::fs::Permissions::from_mode(0o755))
            .tempdir_in(root.path())
            .expect("create scratch dir")
            // The thread-scoped root owns cleanup, so the path can outlive this
            // handle for the rest of the test.
            .keep()
    })
}

mod tests {
    use super::temp_dir;
    use std::fs;

    #[test]
    fn repeated_names_get_distinct_empty_dirs() {
        let first = temp_dir("same");
        let second = temp_dir("same");
        assert_ne!(first, second);
        for dir in [&first, &second] {
            assert_eq!(fs::read_dir(dir).expect("scratch dir exists").count(), 0);
        }
    }

    #[test]
    fn dirs_are_removed_when_their_thread_exits() {
        let dir = std::thread::spawn(|| {
            let dir = temp_dir("cleanup-probe");
            fs::write(dir.join("note.md"), "x").expect("write into scratch dir");
            dir
        })
        .join()
        .expect("probe thread");
        assert!(!dir.exists(), "{} outlived its thread", dir.display());
    }
}
