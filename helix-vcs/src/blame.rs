//! Line based blame ("annotate") information for files tracked by `jj` or `git`.
//!
//! Blame data is obtained by shelling out to the `jj`/`git` CLI. The returned [`FileBlame`] is
//! computed for a *base* text (the last jj snapshot of the working copy or the git working tree
//! file). [`FileBlame::map_lines`] maps the lines of the current (possibly unsaved) document onto
//! the base text and [`remap_lines`] keeps that mapping up to date as the document is edited.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, FixedOffset};
use helix_core::{ChangeSet, Rope};
use imara_diff::{Algorithm, Diff, InternedInput};

/// The VCS used to compute blame information.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlameBackend {
    Jj,
    Git,
}

impl BlameBackend {
    pub fn name(self) -> &'static str {
        match self {
            Self::Jj => "jj",
            Self::Git => "git",
        }
    }
}

/// Which backend(s) are allowed to provide blame information.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BlameBackendPreference {
    /// Use `jj` if the file is inside a `.jj` workspace, otherwise `git`.
    #[default]
    Auto,
    Jj,
    Git,
}

/// Metadata about a single commit (or jj change) that last modified at least one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameCommit {
    /// Full commit hash.
    pub commit_id: String,
    /// Short jj change id. `None` for git.
    pub change_id: Option<String>,
    pub author: String,
    pub email: String,
    /// Unix timestamp (seconds).
    pub time: i64,
    /// Timezone offset of `time` in seconds east of UTC.
    pub tz_offset: i32,
    /// First line of the commit message.
    pub summary: String,
    /// The line is not part of any commit yet (git only).
    pub uncommitted: bool,
}

/// Placeholders understood by [`BlameCommit::format`].
pub const FORMAT_PLACEHOLDERS: &[&str] = &[
    "author",
    "email",
    "date",
    "time-ago",
    "message",
    "id",
    "commit-id",
    "change-id",
];

impl BlameCommit {
    const SHORT_HASH_LEN: usize = 8;

    /// The preferred id: the jj change id if available, the short commit hash otherwise.
    pub fn id(&self) -> &str {
        self.change_id
            .as_deref()
            .unwrap_or_else(|| self.short_commit_id())
    }

    pub fn short_commit_id(&self) -> &str {
        &self.commit_id[..self.commit_id.len().min(Self::SHORT_HASH_LEN)]
    }

    fn datetime(&self) -> Option<DateTime<FixedOffset>> {
        let offset = FixedOffset::east_opt(self.tz_offset)?;
        Some(DateTime::from_timestamp(self.time, 0)?.with_timezone(&offset))
    }

    /// Renders the commit with a format string. Placeholders are written as `{name}`, see
    /// [`FORMAT_PLACEHOLDERS`]. Unknown placeholders are left as is, `{{` and `}}` escape braces.
    pub fn format(&self, format: &str, date_format: &str) -> String {
        if self.uncommitted {
            return "Not committed yet".to_string();
        }
        let mut out = String::with_capacity(format.len() + 64);
        let mut rest = format;
        while let Some(idx) = rest.find(['{', '}']) {
            out.push_str(&rest[..idx]);
            let tail = &rest[idx..];
            if let Some(stripped) = tail.strip_prefix("{{") {
                out.push('{');
                rest = stripped;
                continue;
            }
            if let Some(stripped) = tail.strip_prefix("}}") {
                out.push('}');
                rest = stripped;
                continue;
            }
            if tail.starts_with('{') {
                if let Some(end) = tail.find('}') {
                    let name = &tail[1..end];
                    if self.push_placeholder(&mut out, name, date_format) {
                        rest = &tail[end + 1..];
                        continue;
                    }
                }
            }
            out.push_str(&tail[..1]);
            rest = &tail[1..];
        }
        out.push_str(rest);
        out
    }

    fn push_placeholder(&self, out: &mut String, name: &str, date_format: &str) -> bool {
        use std::fmt::Write;
        match name {
            "author" => out.push_str(&self.author),
            "email" => out.push_str(&self.email),
            "date" => match self.datetime() {
                Some(date) => {
                    // chrono panics on invalid format strings when using `to_string`
                    if write!(out, "{}", date.format(date_format)).is_err() {
                        out.push_str("<invalid date-format>");
                    }
                }
                None => out.push('?'),
            },
            "time-ago" => out.push_str(&time_ago(self.time)),
            "message" if self.summary.is_empty() => out.push_str("(no description set)"),
            "message" => out.push_str(&self.summary),
            "id" => out.push_str(self.id()),
            "commit-id" => out.push_str(self.short_commit_id()),
            "change-id" => out.push_str(self.change_id.as_deref().unwrap_or_default()),
            _ => return false,
        }
        true
    }
}

fn time_ago(time: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(time);
    let secs = (now - time).max(0);
    let (amount, unit) = match secs {
        s if s < 60 => return "just now".to_string(),
        s if s < 60 * 60 => (s / 60, "minute"),
        s if s < 60 * 60 * 24 => (s / (60 * 60), "hour"),
        s if s < 60 * 60 * 24 * 7 => (s / (60 * 60 * 24), "day"),
        s if s < 60 * 60 * 24 * 30 => (s / (60 * 60 * 24 * 7), "week"),
        s if s < 60 * 60 * 24 * 365 => (s / (60 * 60 * 24 * 30), "month"),
        s => (s / (60 * 60 * 24 * 365), "year"),
    };
    let plural = if amount == 1 { "" } else { "s" };
    format!("{amount} {unit}{plural} ago")
}

/// Blame information for a single file.
#[derive(Debug, Clone)]
pub struct FileBlame {
    pub backend: BlameBackend,
    /// Directory the VCS commands are run from.
    pub cwd: PathBuf,
    pub commits: Vec<BlameCommit>,
    /// The commit attributed to lines that don't exist in the base text, i.e. lines that were
    /// changed after the blame was computed. For jj this is the working copy commit, for git
    /// a synthetic "uncommitted" commit.
    pub fallback: u32,
    /// Lines of the base text that was blamed.
    base_lines: Vec<String>,
    /// Commit index for every line of the base text.
    base_commits: Vec<u32>,
}

impl FileBlame {
    /// Maps every line of `text` to a commit index. Lines that differ from the base text are
    /// `None` and should be attributed to [`FileBlame::fallback`].
    pub fn map_lines(&self, text: &Rope) -> Vec<Option<u32>> {
        let mut mapping = vec![None; text.len_lines()];
        let text = text.to_string();
        let base = self.base_lines.concat();
        let input = InternedInput::new(base.as_str(), text.as_str());
        let mut diff = Diff::compute(Algorithm::Histogram, &input);
        diff.postprocess_lines(&input);

        let (mut before, mut after) = (0usize, 0usize);
        let mut map_unchanged = |before: &mut usize, after: &mut usize, until: usize| {
            while *after < until {
                if let (Some(slot), Some(&commit)) =
                    (mapping.get_mut(*after), self.base_commits.get(*before))
                {
                    *slot = Some(commit);
                }
                *before += 1;
                *after += 1;
            }
        };
        for hunk in diff.hunks() {
            map_unchanged(&mut before, &mut after, hunk.after.start as usize);
            before = hunk.before.end as usize;
            after = hunk.after.end as usize;
        }
        map_unchanged(&mut before, &mut after, input.after.len());
        mapping
    }

    pub fn commit(&self, line_mapping: Option<u32>) -> Option<&BlameCommit> {
        self.commits
            .get(line_mapping.unwrap_or(self.fallback) as usize)
    }

    /// Returns `git show`/`jj show` output for the given commit.
    pub fn show_commit(&self, commit: &BlameCommit) -> Result<String> {
        let out = match self.backend {
            BlameBackend::Jj => run_jj(
                &self.cwd,
                [
                    "show",
                    "--git",
                    "-r",
                    &format!("commit_id({})", commit.commit_id),
                ],
            )?,
            BlameBackend::Git if commit.uncommitted => {
                run_git(&self.cwd, ["diff", "--no-color", "--no-ext-diff", "HEAD"])?
            }
            BlameBackend::Git => run_git(
                &self.cwd,
                [
                    "show",
                    "--no-color",
                    "--no-ext-diff",
                    "--format=fuller",
                    &commit.commit_id,
                ],
            )?,
        };
        Ok(String::from_utf8_lossy(&out).into_owned())
    }
}

/// Keeps a line mapping produced by [`FileBlame::map_lines`] in sync with edits to the document.
/// Lines that were edited are mapped to `None`.
pub fn remap_lines(lines: &mut Vec<Option<u32>>, old_text: &Rope, changes: &ChangeSet) {
    if lines.len() != old_text.len_lines() {
        lines.clear();
        return;
    }
    let old_slice = old_text.slice(..);
    let line_end = |line: usize| helix_core::line_ending::line_end_char_index(&old_slice, line);

    let mut new = Vec::with_capacity(lines.len());
    // number of old lines which have been processed
    let mut copied = 0;
    for (from, to, insert) in changes.changes_iter() {
        let start_line = old_text.char_to_line(from);
        let end_line = old_text.char_to_line(to);
        let merges_with_previous = copied > start_line;
        new.extend_from_slice(&lines[copied.min(start_line)..start_line]);

        let insert = insert.as_deref().unwrap_or_default();
        let new_line_count = insert.matches('\n').count();
        let (head, tail) = match (insert.find('\n'), insert.rfind('\n')) {
            (Some(first), Some(last)) => (&insert[..first], &insert[last + 1..]),
            _ => (insert, insert),
        };
        let from_at_line_end = from == line_end(start_line);
        let to_at_line_start = to == old_text.line_to_char(end_line);

        if new_line_count == 0 {
            let line = if insert.is_empty()
                && to_at_line_start
                && from == old_text.line_to_char(start_line)
            {
                // whole lines were deleted, the remaining line is untouched
                lines[end_line]
            } else if insert.is_empty() && from_at_line_end && to == line_end(end_line) {
                lines[start_line]
            } else {
                None
            };
            new.push(line);
        } else {
            new.push(if head.is_empty() && from_at_line_end {
                lines[start_line]
            } else {
                None
            });
            new.extend(std::iter::repeat_n(None, new_line_count - 1));
            new.push(if tail.is_empty() && to_at_line_start {
                lines[end_line]
            } else {
                None
            });
        }
        if merges_with_previous {
            // the first line of this change is the last line of the previous change
            let first = new.len() - 1 - new_line_count;
            new[first - 1] = None;
            new.remove(first);
        }
        copied = end_line + 1;
    }
    new.extend_from_slice(&lines[copied.min(lines.len())..]);
    *lines = new;
}

/// Finds the backend to use for `file`. The closest ancestor directory containing a `.jj` or
/// `.git` folder determines the VCS when `preference` is `Auto`.
pub fn detect_backend(file: &Path, preference: BlameBackendPreference) -> Option<BlameBackend> {
    let dir = file.parent()?;
    for ancestor in dir.ancestors() {
        let jj = ancestor.join(".jj").is_dir();
        let git = ancestor.join(".git").exists();
        match preference {
            BlameBackendPreference::Auto if jj => return Some(BlameBackend::Jj),
            BlameBackendPreference::Auto if git => return Some(BlameBackend::Git),
            BlameBackendPreference::Jj if jj => return Some(BlameBackend::Jj),
            BlameBackendPreference::Git if git => return Some(BlameBackend::Git),
            _ => (),
        }
    }
    None
}

/// Computes blame information for `file`. This runs external processes and blocks.
pub fn blame_file(file: &Path, preference: BlameBackendPreference) -> Result<FileBlame> {
    let backend = detect_backend(file, preference).context("file is not inside a repository")?;
    let cwd = file.parent().context("file has no parent directory")?;
    let file_name = file.file_name().context("path has no file name")?;
    let res = match backend {
        BlameBackend::Jj => jj_blame(cwd, file_name),
        BlameBackend::Git => git_blame(cwd, file_name),
    };
    match res {
        // a colocated jj repository can still be blamed by git if jj is unavailable
        Err(err)
            if backend == BlameBackend::Jj
                && preference == BlameBackendPreference::Auto
                && detect_backend(file, BlameBackendPreference::Git).is_some() =>
        {
            log::debug!("jj blame failed, falling back to git: {err:#}");
            git_blame(cwd, file_name)
        }
        res => res,
    }
}

fn run(
    program: &str,
    cwd: &Path,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
) -> Result<Vec<u8>> {
    let output = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to run {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

fn run_jj(cwd: &Path, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Result<Vec<u8>> {
    // `--ignore-working-copy` prevents creating a new operation (and taking the working copy
    // lock) every time a file is blamed. Changes which haven't been snapshotted yet are mapped
    // to the working copy commit by `FileBlame::map_lines`.
    let global: [&OsStr; 4] = [
        "--no-pager".as_ref(),
        "--color=never".as_ref(),
        "--ignore-working-copy".as_ref(),
        "--quiet".as_ref(),
    ];
    let args: Vec<_> = args
        .into_iter()
        .map(|arg| arg.as_ref().to_owned())
        .collect();
    run(
        "jj",
        cwd,
        global.iter().map(|arg| arg.to_os_string()).chain(args),
    )
}

fn run_git(cwd: &Path, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Result<Vec<u8>> {
    run("git", cwd, args)
}

const JJ_COMMIT_TEMPLATE: &str = r#"commit_id ++ "\x1f" ++ change_id.short(8) ++ "\x1f" ++ author.name() ++ "\x1f" ++ author.email() ++ "\x1f" ++ author.timestamp().format("%s %z") ++ "\x1f" ++ description.first_line() ++ "\x1f" ++ if(current_working_copy, "1", "0") ++ "\x1e""#;

fn jj_blame(cwd: &Path, file_name: &OsStr) -> Result<FileBlame> {
    let annotate = run_jj(
        cwd,
        [
            "file".as_ref(),
            "annotate".as_ref(),
            "-r".as_ref(),
            "@".as_ref(),
            "-T".as_ref(),
            r#"commit.commit_id() ++ "\0" ++ content"#.as_ref(),
            file_name,
        ],
    );
    let annotate = match annotate {
        Ok(annotate) => annotate,
        // the file hasn't been snapshotted yet, all lines belong to the working copy
        Err(err) if err.to_string().contains("No such path") => Vec::new(),
        Err(err) => return Err(err),
    };
    let annotate = String::from_utf8_lossy(&annotate);

    let mut base_lines = Vec::new();
    let mut line_commit_ids = Vec::new();
    let mut rest = &*annotate;
    while !rest.is_empty() {
        let (commit_id, after) = rest
            .split_once('\0')
            .context("malformed jj annotate output")?;
        let line_len = after.find('\n').map_or(after.len(), |idx| idx + 1);
        line_commit_ids.push(commit_id);
        base_lines.push(after[..line_len].to_string());
        rest = &after[line_len..];
    }

    let mut unique_ids: Vec<&str> = line_commit_ids.clone();
    unique_ids.sort_unstable();
    unique_ids.dedup();
    let revset = std::iter::once("@".to_string())
        .chain(unique_ids.iter().map(|id| format!("commit_id({id})")))
        .collect::<Vec<_>>()
        .join("|");
    let log = run_jj(
        cwd,
        ["log", "--no-graph", "-r", &revset, "-T", JJ_COMMIT_TEMPLATE],
    )?;
    let log = String::from_utf8_lossy(&log);

    let mut commits = Vec::new();
    let mut fallback = None;
    for record in log.split('\x1e').filter(|record| !record.trim().is_empty()) {
        let fields: Vec<_> = record.split('\x1f').collect();
        let [commit_id, change_id, author, email, timestamp, summary, working_copy] = fields[..]
        else {
            bail!("malformed jj log output: {record:?}");
        };
        let (time, tz_offset) = parse_jj_timestamp(timestamp).unwrap_or_default();
        if working_copy == "1" {
            fallback = Some(commits.len() as u32);
        }
        commits.push(BlameCommit {
            commit_id: commit_id.to_string(),
            change_id: Some(change_id.to_string()),
            author: author.to_string(),
            email: email.to_string(),
            time,
            tz_offset,
            summary: summary.to_string(),
            uncommitted: false,
        });
    }

    let base_commits = line_commit_ids
        .iter()
        .map(|id| {
            commits
                .iter()
                .position(|commit| commit.commit_id == *id)
                .map(|idx| idx as u32)
                .context("commit missing from jj log output")
        })
        .collect::<Result<_>>()?;

    Ok(FileBlame {
        backend: BlameBackend::Jj,
        cwd: cwd.to_path_buf(),
        commits,
        fallback: fallback.context("working copy commit not found")?,
        base_lines,
        base_commits,
    })
}

/// Parses `"<unix seconds> <+HHMM>"`.
fn parse_jj_timestamp(timestamp: &str) -> Option<(i64, i32)> {
    let (secs, tz) = timestamp.split_once(' ')?;
    Some((secs.parse().ok()?, parse_tz(tz)?))
}

/// Parses a timezone in the form `+HHMM`/`-HHMM` into seconds.
fn parse_tz(tz: &str) -> Option<i32> {
    let sign = match tz.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digits = tz.get(1..5)?;
    let hours: i32 = digits[..2].parse().ok()?;
    let minutes: i32 = digits[2..].parse().ok()?;
    Some(sign * (hours * 3600 + minutes * 60))
}

fn git_blame(cwd: &Path, file_name: &OsStr) -> Result<FileBlame> {
    let out = run_git(
        cwd,
        [
            "blame".as_ref(),
            "--porcelain".as_ref(),
            "--".as_ref(),
            file_name,
        ],
    )?;
    let out = String::from_utf8_lossy(&out);
    parse_git_porcelain(&out, cwd)
}

fn parse_git_porcelain(out: &str, cwd: &Path) -> Result<FileBlame> {
    let mut commits: Vec<BlameCommit> = Vec::new();
    let mut base_lines = Vec::new();
    let mut base_commits = Vec::new();
    let mut current: Option<usize> = None;

    let mut rest = out;
    while !rest.is_empty() {
        let line_len = rest.find('\n').map_or(rest.len(), |idx| idx + 1);
        let line = &rest[..line_len];
        rest = &rest[line_len..];

        if let Some(content) = line.strip_prefix('\t') {
            let idx = current.context("malformed git blame output")?;
            base_lines.push(content.to_string());
            base_commits.push(idx as u32);
            continue;
        }
        let line = line.trim_end_matches(['\n', '\r']);
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        let is_header = key.len() >= 40 && key.bytes().all(|b| b.is_ascii_hexdigit());
        if is_header {
            current = Some(
                commits
                    .iter()
                    .position(|commit| commit.commit_id == key)
                    .unwrap_or_else(|| {
                        commits.push(BlameCommit {
                            commit_id: key.to_string(),
                            change_id: None,
                            author: String::new(),
                            email: String::new(),
                            time: 0,
                            tz_offset: 0,
                            summary: String::new(),
                            uncommitted: key.bytes().all(|b| b == b'0'),
                        });
                        commits.len() - 1
                    }),
            );
            continue;
        }
        let Some(commit) = current.map(|idx| &mut commits[idx]) else {
            continue;
        };
        match key {
            "author" => commit.author = value.to_string(),
            "author-mail" => {
                commit.email = value
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_string()
            }
            "author-time" => commit.time = value.parse().unwrap_or_default(),
            "author-tz" => commit.tz_offset = parse_tz(value).unwrap_or_default(),
            "summary" => commit.summary = value.to_string(),
            _ => (),
        }
    }

    // git blame doesn't attach a newline to the last line, but the working tree file might have
    // one which we can't know. Missing newlines only affect the last line of the diff mapping.
    let fallback = match commits.iter().position(|commit| commit.uncommitted) {
        Some(idx) => idx,
        None => {
            commits.push(BlameCommit {
                commit_id: "0".repeat(40),
                change_id: None,
                author: String::new(),
                email: String::new(),
                time: 0,
                tz_offset: 0,
                summary: String::new(),
                uncommitted: true,
            });
            commits.len() - 1
        }
    };

    Ok(FileBlame {
        backend: BlameBackend::Git,
        cwd: cwd.to_path_buf(),
        commits,
        fallback: fallback as u32,
        base_lines,
        base_commits,
    })
}

#[cfg(test)]
mod test {
    use super::*;
    use helix_core::Transaction;

    fn blame(base: &str) -> FileBlame {
        let base_lines: Vec<String> = base.split_inclusive('\n').map(str::to_string).collect();
        FileBlame {
            backend: BlameBackend::Git,
            cwd: PathBuf::new(),
            commits: Vec::new(),
            fallback: 0,
            base_commits: (0..base_lines.len() as u32).collect(),
            base_lines,
        }
    }

    #[test]
    fn map_lines() {
        let blame = blame("a\nb\nc\nd\n");
        let text = Rope::from("a\nx\nb\nd\ny\n");
        assert_eq!(
            blame.map_lines(&text),
            vec![Some(0), None, Some(1), Some(3), None, None]
        );
    }

    fn apply(text: &str, changes: &[(usize, usize, Option<&str>)]) -> Vec<Option<u32>> {
        let rope = Rope::from(text);
        let mut lines = blame(text).map_lines(&rope);
        let transaction = Transaction::change(
            &rope,
            changes
                .iter()
                .map(|&(from, to, ins)| (from, to, ins.map(Into::into))),
        );
        remap_lines(&mut lines, &rope, transaction.changes());
        let mut new = rope.clone();
        transaction.apply(&mut new);
        assert_eq!(lines.len(), new.len_lines());
        lines
    }

    #[test]
    fn remap() {
        let text = "aa\nbb\ncc\n";
        // edit inside a line
        assert_eq!(
            apply(text, &[(4, 4, Some("x"))]),
            vec![Some(0), None, Some(2), None]
        );
        // open a line below `aa`
        assert_eq!(
            apply(text, &[(2, 2, Some("\nnew"))]),
            vec![Some(0), None, Some(1), Some(2), None]
        );
        // insert a line above `bb`
        assert_eq!(
            apply(text, &[(3, 3, Some("new\n"))]),
            vec![Some(0), None, Some(1), Some(2), None]
        );
        // delete `bb\n`
        assert_eq!(apply(text, &[(3, 6, None)]), vec![Some(0), Some(2), None]);
        // join `aa` and `bb`
        assert_eq!(apply(text, &[(2, 3, None)]), vec![None, Some(2), None]);
        // delete `\nbb`
        assert_eq!(apply(text, &[(2, 5, None)]), vec![Some(0), Some(2), None]);
        // split a line
        assert_eq!(
            apply(text, &[(4, 4, Some("\n"))]),
            vec![Some(0), None, None, Some(2), None]
        );
        // multiple cursors on the same line
        assert_eq!(
            apply(text, &[(3, 3, Some("x")), (4, 4, Some("y"))]),
            vec![Some(0), None, Some(2), None]
        );
        // multiple cursors on different lines
        assert_eq!(
            apply(text, &[(0, 0, Some("x")), (6, 6, Some("new\n"))]),
            vec![None, Some(1), None, Some(2), None]
        );
    }

    #[test]
    fn format() {
        let commit = BlameCommit {
            commit_id: "0123456789abcdef".into(),
            change_id: Some("zzxxyyww".into()),
            author: "Jane".into(),
            email: "jane@example.com".into(),
            time: 0,
            tz_offset: 3600,
            summary: "fix things".into(),
            uncommitted: false,
        };
        assert_eq!(
            commit.format(
                "{author} - {date}: {message} ({id}) {{x}} {unknown}",
                "%Y-%m-%d %H:%M"
            ),
            "Jane - 1970-01-01 01:00: fix things (zzxxyyww) {x} {unknown}"
        );
        assert_eq!(commit.format("{commit-id}", ""), "01234567");
    }

    #[test]
    fn porcelain() {
        let out = "\
1111111111111111111111111111111111111111 1 1 2
author Jane
author-mail <jane@example.com>
author-time 100
author-tz -0130
summary first
filename f
\tline one
1111111111111111111111111111111111111111 2 2
\tline two
0000000000000000000000000000000000000000 3 3 1
author Not Committed Yet
summary Version of f from f
filename f
\tline three
";
        let blame = parse_git_porcelain(out, Path::new("")).unwrap();
        assert_eq!(blame.base_commits, vec![0, 0, 1]);
        assert_eq!(blame.fallback, 1);
        assert_eq!(blame.commits[0].tz_offset, -5400);
        assert_eq!(blame.commits[0].email, "jane@example.com");
        assert!(blame.commits[1].uncommitted);
    }
}
