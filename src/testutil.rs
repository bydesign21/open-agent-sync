//! Shared test helpers.
//!
//! Exists for one reason: `AGENTSYNC_HOME` is process-global, and several test
//! modules redirect it. Two tests doing this concurrently point each other's
//! config directory at a `TempDir` that is about to be dropped. This produces a
//! failure that looks like a bug in the code under test. A lock per module does
//! not solve this problem. The fix needs one shared lock, so it lives in this
//! file.

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
        // Poisoning means only that an earlier test panicked. This lock is still safe to use.
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
