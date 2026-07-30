//! Shared test helpers.
//!
//! Exists for one reason: `AGENTSYNC_HOME` is process-global, and several test
//! modules redirect it. Two tests doing that concurrently point each other's
//! config directory at a `TempDir` that is about to be dropped, which fails in a
//! way that looks like a bug in the code under test. A lock per module would not
//! help — it has to be one lock, so it lives here.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

static HOME_LOCK: Mutex<()> = Mutex::new(());

/// A temporary `AGENTSYNC_HOME`, held for as long as this value lives.
pub struct TmpHome {
    dir: tempfile::TempDir,
    _guard: MutexGuard<'static, ()>,
}

impl TmpHome {
    pub fn new() -> Self {
        // Poisoning only means an earlier test panicked; the lock is still ours.
        let guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("temp dir");
        unsafe { std::env::set_var("AGENTSYNC_HOME", dir.path()) };
        TmpHome { dir, _guard: guard }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }
}

impl Default for TmpHome {
    fn default() -> Self {
        Self::new()
    }
}
