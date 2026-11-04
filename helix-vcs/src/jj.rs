//! Listing changed files via the `jj` (Jujutsu) CLI.
//!
//! The git provider's [`crate::DiffProviderRegistry::for_each_changed_file`] compares git's index
//! to the worktree. In a colocated jj repository that comparison is wrong: jj rewrites working-copy
//! files without refreshing git's index stat cache, so git (and therefore gix) report dozens of
//! unchanged files as modified. jj is the authority on what the working-copy change contains, so
//! when a `.jj` directory is present we ask jj directly. This matches `jj status` and the diff
//! gutters, which diff the working copy against its parent commit rather than the git index.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::FileChange;

/// Returns the closest ancestor of `path` (inclusive) that is a jj workspace root, i.e. one that
/// contains a `.jj` directory.
pub fn workspace_root(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|ancestor| ancestor.join(".jj").is_dir())
        .map(Path::to_path_buf)
}

// One record per changed file, `<status>\0<source path>\0<target path>\n`. NUL separates the
// fields so paths containing spaces or tabs survive intact, and the full source/target paths avoid
// jj's `{a => b}` rename abbreviation. Paths are relative to the workspace root.
//
// Conflicted files are listed first, because jj reports a conflict even when the commit has no diff
// against its parents (e.g. a merge commit whose content equals the auto-merged trees), which
// `self.diff()` alone would miss.
const CHANGED_FILES_TEMPLATE: &str = concat!(
    r#"self.conflicted_files().map(|p| "conflict\x00" ++ p.path() ++ "\x00" ++ p.path() ++ "\n").join("")"#,
    " ++ ",
    r#"self.diff().files().map(|f| f.status() ++ "\x00" ++ f.source().path() ++ "\x00" ++ f.target().path() ++ "\n").join("")"#,
);

/// Reports every file changed in the working-copy commit (`@`) relative to its parent, the same set
/// `jj status` shows. Keeps iterating until `f` returns `false`.
pub fn for_each_changed_file(cwd: &Path, f: impl Fn(Result<FileChange>) -> bool) -> Result<()> {
    let root = workspace_root(cwd).context("not inside a jj workspace")?;

    // No `--ignore-working-copy`: `jj log` snapshots the working copy first, so the listing
    // reflects the files on disk right now, exactly as `jj status` would.
    let args: [&OsStr; 6] = [
        "log".as_ref(),
        "--no-graph".as_ref(),
        "-r".as_ref(),
        "@".as_ref(),
        "-T".as_ref(),
        CHANGED_FILES_TEMPLATE.as_ref(),
    ];
    let output = run_jj(&root, args)?;
    let output = String::from_utf8(output).context("jj emitted invalid UTF-8")?;

    let mut conflicted = HashSet::new();
    for record in output.split('\n').filter(|record| !record.is_empty()) {
        let mut fields = record.split('\0');
        let (Some(status), Some(source), Some(target)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let source = root.join(source);
        let target = root.join(target);
        let change = match status {
            "conflict" => {
                conflicted.insert(target.clone());
                FileChange::Conflict { path: target }
            }
            // A conflicted file can also appear in the diff; show it once, as a conflict.
            _ if conflicted.contains(&target) => continue,
            "added" | "copied" => FileChange::Untracked { path: target },
            "removed" => FileChange::Deleted { path: source },
            "renamed" => FileChange::Renamed {
                from_path: source,
                to_path: target,
            },
            // "modified", and anything a future jj adds, is shown as a plain modification.
            _ => FileChange::Modified { path: target },
        };
        if !f(Ok(change)) {
            break;
        }
    }

    Ok(())
}

fn run_jj(cwd: &Path, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Result<Vec<u8>> {
    let globals: [&OsStr; 3] = [
        "--no-pager".as_ref(),
        "--color=never".as_ref(),
        "--quiet".as_ref(),
    ];
    let output = Command::new("jj")
        .args(globals)
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .context("failed to run jj")?;
    if !output.status.success() {
        bail!(
            "jj failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}
