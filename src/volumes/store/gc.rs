//! Store lock and garbage collection.

use std::fs;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::SystemTime;

use super::{
    generations, mkdir_private, plan_gc, remove, subdirs, unix_ns, GcReport, GroupSnapshot, Store,
    LEASES_DIR, LOCK_FILE, USED_FILE,
};

impl Store {
    /// Takes the store lock, shared for clone and publish, exclusive for GC.
    pub(super) fn lock(&self, exclusive: bool) -> Result<fs::File, String> {
        let dir = self.root.join(LEASES_DIR);
        mkdir_private(&dir)?;
        let file = fs::File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(dir.join(LOCK_FILE))
            .map_err(|e| format!("open store lock: {}", e.kind()))?;
        let locked = if exclusive {
            file.lock()
        } else {
            file.lock_shared()
        };
        locked.map_err(|e| format!("take store lock: {}", e.kind()))?;
        Ok(file)
    }

    /// Every group of every compat tag, for GC.
    fn snapshot(&self) -> Vec<GroupSnapshot> {
        let leases = self.root.join(LEASES_DIR);
        subdirs(&self.root)
            .into_iter()
            .filter(|d| *d != leases)
            .flat_map(|compat| subdirs(&compat))
            .map(|dir| {
                let used = fs::metadata(dir.join(USED_FILE))
                    .or_else(|_| fs::metadata(&dir))
                    .and_then(|m| m.modified())
                    .map_or(0, unix_ns);
                GroupSnapshot {
                    gens: generations(&dir),
                    dir,
                    used,
                }
            })
            .collect()
    }

    /// Removes working copies of leases `live` rejects, then what [`plan_gc`]
    /// picks. `dry_run` only reports.
    pub fn gc(&self, now: SystemTime, live: &dyn Fn(&str) -> bool, dry_run: bool) -> GcReport {
        let _lock = match self.lock(true) {
            Ok(lock) => lock,
            Err(e) => {
                return GcReport {
                    removed: Vec::new(),
                    errors: vec![e],
                }
            }
        };
        let mut removed: Vec<PathBuf> = subdirs(&self.root.join(LEASES_DIR))
            .into_iter()
            .filter(|d| {
                d.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| !live(n))
            })
            .collect();
        removed.extend(plan_gc(
            &self.snapshot(),
            unix_ns(now),
            self.ttl,
            self.budget,
        ));
        let mut errors = Vec::new();
        if !dry_run {
            removed.retain(|p| remove(p).map_err(|e| errors.push(e)).is_ok());
        }
        GcReport { removed, errors }
    }
}
