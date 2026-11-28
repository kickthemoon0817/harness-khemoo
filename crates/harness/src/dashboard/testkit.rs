//! Fixtures and scratch directories for the dashboard's tests.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

pub static REPO_ROOT: LazyLock<PathBuf> = LazyLock::new(|| {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap_or_default()
});
pub static FIXTURES: LazyLock<PathBuf> =
    LazyLock::new(|| REPO_ROOT.join("tests/fixtures/dashboard"));
pub static FLEET_FIXTURES: LazyLock<PathBuf> =
    LazyLock::new(|| REPO_ROOT.join("tests/fixtures/fleet"));

pub fn fixture_text(name: &str) -> String {
    std::fs::read_to_string(FIXTURES.join(name))
        .unwrap_or_else(|error| panic!("fixture {name}: {error}"))
}

pub fn fleet_fixture_text(name: &str) -> String {
    std::fs::read_to_string(FLEET_FIXTURES.join(name))
        .unwrap_or_else(|error| panic!("fixture {name}: {error}"))
}

pub fn fixture_json(name: &str) -> serde_json::Value {
    serde_json::from_str(&fixture_text(name))
        .unwrap_or_else(|error| panic!("fixture {name}: {error}"))
}

/// A directory removed with everything in it when dropped; its path is resolved, so it compares
/// equal to what the process table and `readlink` report.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let name = format!(
            "harness-dashboard-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        );
        let path = std::env::temp_dir().join(name);
        std::fs::create_dir_all(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        Self(path.canonicalize().unwrap_or(path))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

pub fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).ok();
    for entry in std::fs::read_dir(from).into_iter().flatten().flatten() {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).ok();
        }
    }
}

pub fn set_mtime(path: &Path, epoch: f64) {
    let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs_f64(epoch);
    if let Ok(file) = std::fs::File::options().write(true).open(path) {
        file.set_modified(time).ok();
    }
}

pub fn write_script(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).ok();
}
