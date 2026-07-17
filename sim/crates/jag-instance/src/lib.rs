//! Multi-instance isolation — the antidote to BigPEmu's single global
//! `/tmp/bigpemu-shared/.lock` that serialized every Claude through one
//! emulator.
//!
//! There is **no global lock here**. Each emulator invocation is an ordinary,
//! fully-isolated process with its own state directory under
//! `$JAGEMU_HOME/instances/<id>/`. N Claude instances run N emulators
//! concurrently with zero contention. This module only allocates the isolated
//! directory + a discovery registry; the emulation itself is in-process and
//! single-threaded, so isolation is inherent (separate processes, separate
//! memory) — we never need mutual exclusion.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process;

/// Root directory holding all instances. Honors `$JAGEMU_HOME`, else
/// `~/.jagemu`, else `/tmp/jagemu-<uid>`.
pub fn home() -> PathBuf {
    if let Ok(h) = std::env::var("JAGEMU_HOME") {
        return PathBuf::from(h);
    }
    if let Ok(h) = std::env::var("HOME") {
        return PathBuf::from(h).join(".jagemu");
    }
    PathBuf::from("/tmp/jagemu")
}

/// A live, isolated emulator instance directory.
#[derive(Debug, Clone)]
pub struct Instance {
    pub id: String,
    pub dir: PathBuf,
    pub project: String,
    pub pid: u32,
}

impl Instance {
    /// Allocate a fresh, collision-free instance for `project`.
    ///
    /// The id is `<project>-<pid>` (optionally suffixed if that somehow exists),
    /// which is unique across parallel Claude instances because each runs as a
    /// distinct process. No locking, no waiting.
    pub fn create(project: &str) -> io::Result<Instance> {
        let project = sanitize(project);
        let pid = process::id();
        let base = home().join("instances");
        fs::create_dir_all(&base)?;

        let mut id = format!("{project}-{pid}");
        let mut n = 1;
        while base.join(&id).exists() {
            id = format!("{project}-{pid}-{n}");
            n += 1;
        }
        let dir = base.join(&id);
        fs::create_dir_all(dir.join("screenshots"))?;
        fs::create_dir_all(dir.join("state"))?;

        let inst = Instance { id, dir: dir.clone(), project, pid };
        inst.write_meta()?;
        Ok(inst)
    }

    /// Path to this instance's control socket (used by the daemon mode).
    pub fn control_socket(&self) -> PathBuf {
        self.dir.join("control.sock")
    }

    /// Per-instance scratch path for an output artifact.
    pub fn artifact(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn write_meta(&self) -> io::Result<()> {
        let meta = format!(
            "{{\"id\":\"{}\",\"project\":\"{}\",\"pid\":{},\"dir\":\"{}\"}}\n",
            self.id,
            self.project,
            self.pid,
            self.dir.display()
        );
        fs::write(self.dir.join("meta.json"), meta)
    }

    /// Remove this instance's directory (called on clean shutdown).
    pub fn cleanup(&self) -> io::Result<()> {
        if self.dir.exists() {
            fs::remove_dir_all(&self.dir)?;
        }
        Ok(())
    }

    /// Is the owning process still alive? Used to prune stale instances —
    /// the kernel-reaped equivalent of BigPEmu's flock auto-release, but with
    /// no lock involved.
    pub fn is_alive(&self) -> bool {
        pid_alive(self.pid)
    }
}

/// List all registered instances (live and stale).
pub fn list() -> io::Result<Vec<InstanceInfo>> {
    let base = home().join("instances");
    let mut out = Vec::new();
    if !base.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(&base)? {
        let entry = entry?;
        let meta = entry.path().join("meta.json");
        if let Ok(s) = fs::read_to_string(&meta) {
            let pid = json_u32(&s, "pid").unwrap_or(0);
            out.push(InstanceInfo {
                id: json_str(&s, "id").unwrap_or_default(),
                project: json_str(&s, "project").unwrap_or_default(),
                pid,
                dir: entry.path(),
                alive: pid_alive(pid),
            });
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

/// Remove instance directories whose owning process is gone.
pub fn prune_stale() -> io::Result<usize> {
    let mut removed = 0;
    for info in list()? {
        if !info.alive {
            let _ = fs::remove_dir_all(&info.dir);
            removed += 1;
        }
    }
    Ok(removed)
}

#[derive(Debug, Clone)]
pub struct InstanceInfo {
    pub id: String,
    pub project: String,
    pub pid: u32,
    pub dir: PathBuf,
    pub alive: bool,
}

fn sanitize(s: &str) -> String {
    let s = if s.is_empty() { "jag" } else { s };
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    Path::new(&format!("/proc/{pid}")).exists()
}

// Minimal JSON field extraction for our own fixed-shape meta files.
fn json_str(s: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let i = s.find(&needle)? + needle.len();
    let rest = &s[i..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn json_u32(s: &str, key: &str) -> Option<u32> {
    let needle = format!("\"{key}\":");
    let i = s.find(&needle)? + needle.len();
    let rest = &s[i..];
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}
