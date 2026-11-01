//! Stable, test-owned executables for the independent ACP stdio peer.

use std::path::{Path, PathBuf};

pub fn executable(root: &Path) -> PathBuf {
    let name = format!("bee-acp-client-fixture{}", std::env::consts::EXE_SUFFIX);
    let snapshot = root.join(&name);
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    // Keep nested Cargo builds separate from the parent test build's target lock.
    let target = manifest.join("target/acp-client-fixture");
    std::fs::create_dir_all(&target).unwrap();
    // All test processes must lock the same persistent inode, including while copying.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(target.join("build-snapshot.lock"))
        .unwrap();
    fs2::FileExt::lock_exclusive(&lock).unwrap();
    // Multiple agents in one test share a snapshot that must never be overwritten.
    if snapshot.is_file() {
        return snapshot;
    }
    let output = std::process::Command::new(env!("CARGO"))
        .args(["build", "--offline", "--locked", "--manifest-path"])
        .arg(manifest.join("tests/fixtures/acp-agent/Cargo.toml"))
        .arg("--target-dir")
        .arg(&target)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // On macOS even fresh Cargo builds remove and recopy the public executable.
    // Snapshot under the lock so another build cannot interrupt copying or a later launch.
    std::fs::copy(target.join("debug").join(name), &snapshot).unwrap();
    snapshot
}
