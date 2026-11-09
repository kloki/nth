mod apply;
mod parse;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    io::ErrorKind,
    path::{Path, PathBuf},
};

use apply::apply;
use futures::{
    FutureExt,
    future::{BoxFuture, join_all},
};
use nth_lsp::report;
use nth_protocol::{Tool, ToolContext, ToolResult, ToolSpec};
use parse::Section;
use serde::Deserialize;
use serde_json::json;

use crate::{
    PostWrite,
    post_write::{self, write_atomic},
};

pub struct ApplyPatch {
    post_write: PostWrite,
}

impl ApplyPatch {
    pub fn new(post_write: PostWrite) -> Self {
        Self { post_write }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Args {
    patch_text: String,
}

impl Tool for ApplyPatch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "apply_patch",
            description: include_str!("description.txt").into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "patchText": { "type": "string", "description": "The full patch text that describes all changes to be made" }
                },
                "required": ["patchText"]
            }),
        }
    }

    fn call<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolResult> {
        async move {
            let args: Args = nth_protocol::parse_args(args)?;
            let sections =
                parse::parse(&args.patch_text).map_err(|e| format!("invalid patch: {e}"))?;
            let paths: Vec<_> = sections
                .iter()
                .flat_map(|section| {
                    let (path, destination) = match section {
                        Section::Add { path, .. } | Section::Delete { path } => (path, None),
                        Section::Update { path, move_to, .. } => (path, move_to.as_ref()),
                    };
                    std::iter::once(path)
                        .chain(destination)
                        .map(|p| crate::path::resolve(&ctx.cwd, p))
                })
                .collect();
            let (writing, identities) = self.lock(&paths).await?;
            let mut plan = Plan::new(&ctx.cwd, identities);
            let mut summary = Vec::with_capacity(sections.len());
            for section in &sections {
                let line = plan
                    .add(section)
                    .await
                    .map_err(|e| format!("{e}\nNo files were changed."))?;
                summary.push(line);
            }
            // Checked before anything is written, so a refused patch
            // changes no file at all. Every name the patch uses is checked,
            // as the write tool checks the one it is given.
            for path in &paths {
                ctx.writable
                    .check(path)
                    .map_err(|e| format!("{e}\nNo files were changed."))?;
            }
            let changed = plan.changed();
            plan.write().await?;
            let mut notes = Vec::new();
            for path in &changed {
                let name = relative(path, &ctx.cwd);
                let ran = writing.format(path, &ctx.cwd).await;
                notes.extend(ran.lines().map(|note| format!("{name}: {note}")));
            }
            drop(writing);
            let diagnostics = join_all(
                changed
                    .iter()
                    .map(|p| self.post_write.diagnostics(p, post_write::caller(ctx))),
            )
            .await;
            let errors: Vec<String> = changed
                .iter()
                .zip(&diagnostics)
                .map(|(path, d)| report::after_patch(&relative(path, &ctx.cwd), path, d))
                .collect();
            let done = format!(
                "Success. Updated the following files:\n{}",
                summary.join("\n")
            );
            let notes = notes.join("\n");
            let mut sections = vec![notes.as_str()];
            sections.extend(errors.iter().map(String::as_str));
            Ok(post_write::append(done, &sections))
        }
        .boxed()
    }
}

/// How many times a patch waits for its files' locks again when what its
/// paths name moved while it waited, before it gives up.
const LOCK_TRIES: usize = 5;

impl ApplyPatch {
    /// Locks what `paths` name and returns it, resolved under the lock. A
    /// symlink's own identity is locked too, since a delete or a move takes
    /// the link itself. What a path names can change while the patch waits
    /// (another patch deleting a link), and a plan made from the stale
    /// answer would delete or write the file the link used to name, so it
    /// is asked again once the lock is held, and on a change the lock goes
    /// and the patch waits for the new set.
    async fn lock(
        &self,
        paths: &[PathBuf],
    ) -> Result<(post_write::Writing<'_>, HashMap<PathBuf, Identity>), String> {
        let mut resolved = identities(paths.to_vec()).await?;
        for _ in 0..LOCK_TRIES {
            let locked = resolved
                .values()
                .flat_map(|identity| [identity.file.clone(), identity.own.clone()])
                .collect();
            let writing = self.post_write.lock_identities(locked).await;
            let now = identities(paths.to_vec()).await?;
            if now == resolved {
                return Ok((writing, now));
            }
            drop(writing);
            resolved = now;
        }
        Err("the files this patch names kept changing while it waited for them; try again.\nNo files were changed.".into())
    }
}

/// `path` as the model named it, relative to the working directory.
fn relative(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).display().to_string()
}

/// The file each path names, keyed by the path: `a.txt`, `sub/../a.txt` and
/// a symlink to it are one file, as [`PostWrite::lock`] sees them.
async fn identities(paths: Vec<PathBuf>) -> Result<HashMap<PathBuf, Identity>, String> {
    // `identity` asks the filesystem about every component.
    tokio::task::spawn_blocking(move || {
        paths
            .into_iter()
            .map(|path| {
                let identity = Identity::of(&path);
                (path, identity)
            })
            .collect()
    })
    .await
    .map_err(|e| format!("cannot resolve the patch's paths: {e}"))
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct Identity {
    /// The file the path names, through a symlink at its end.
    file: PathBuf,
    /// The path's last component itself, which differs from `file` only
    /// when that component is a symlink.
    own: PathBuf,
}

impl Identity {
    fn of(path: &Path) -> Self {
        let file = post_write::identity(path);
        let own = match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) => post_write::identity(parent).join(name),
            _ => file.clone(),
        };
        Self { file, own }
    }
}

/// What a patch does to each file, worked out in memory before any file is
/// written, so a hunk that fails to apply leaves every file as it was.
struct Plan<'a> {
    cwd: &'a Path,
    /// The file each path the patch names is, from [`identities`].
    identities: HashMap<PathBuf, Identity>,
    /// What becomes of each file, keyed by its identity, so two names for
    /// one file are one entry. Sections read what earlier ones planned, so
    /// a patch may touch a file twice.
    files: BTreeMap<PathBuf, Planned>,
    /// The files that were there before the patch, so a file it adds and
    /// then deletes is never asked to go.
    on_disk: BTreeSet<PathBuf>,
}

struct Planned {
    /// The path the patch first named the file by, for what the model reads.
    name: PathBuf,
    /// Its new content, or `None` to delete it.
    content: Option<String>,
    /// A symlink that goes before anything is written: deleted, and then
    /// perhaps added again as a file of its own.
    unlink: bool,
}

/// A file the patch named: the path as named, and the file it is.
struct Named {
    name: PathBuf,
    identity: PathBuf,
    /// The path's own last component, a symlink where it differs.
    own: PathBuf,
}

impl<'a> Plan<'a> {
    fn new(cwd: &'a Path, identities: HashMap<PathBuf, Identity>) -> Self {
        Self {
            cwd,
            identities,
            files: BTreeMap::new(),
            on_disk: BTreeSet::new(),
        }
    }

    fn named(&self, raw: &str) -> Named {
        let name = crate::path::resolve(self.cwd, raw);
        // Every path a section names was resolved before planning; a miss
        // would be a path the tool forgot to collect, keyed as named.
        let identity = self.identities.get(&name).cloned();
        let identity = identity.unwrap_or_else(|| Identity {
            file: name.clone(),
            own: name.clone(),
        });
        Named {
            name,
            identity: identity.file,
            own: identity.own,
        }
    }

    /// Where the plan keeps `file`: under the symlink itself once a section
    /// planned the link gone, so later sections see it gone, or added anew
    /// as a file of its own; else under the file it names.
    fn key(&self, file: &Named) -> PathBuf {
        if file.own != file.identity && self.files.contains_key(&file.own) {
            file.own.clone()
        } else {
            file.identity.clone()
        }
    }

    /// Plans `file` gone. A symlink goes itself, as `rm` and git remove
    /// one, never the file it names, which may lie outside what this
    /// session may write.
    async fn remove(&mut self, file: Named) {
        let link = file.own != file.identity && !self.files.contains_key(&file.own);
        if link && tokio::fs::symlink_metadata(&file.own).await.is_ok() {
            let planned = Planned {
                name: file.name,
                content: None,
                unlink: true,
            };
            self.files.insert(file.own, planned);
        } else {
            self.set(file, None);
        }
    }

    fn set(&mut self, file: Named, content: Option<String>) {
        let entry = self.files.entry(self.key(&file)).or_insert(Planned {
            name: file.name,
            content: None,
            unlink: false,
        });
        entry.content = content;
    }

    /// Plans one section and returns its line of the summary.
    async fn add(&mut self, section: &Section) -> Result<String, String> {
        match section {
            Section::Add { path, content } => {
                let file = self.named(path);
                if self.read(&file).await?.is_some() {
                    return Err(format!(
                        "cannot add {}: it already exists. Use `*** Update File:` to change it.",
                        file.name.display()
                    ));
                }
                self.set(file, Some(content.clone()));
                Ok(format!("A {path}"))
            }
            Section::Delete { path } => {
                let file = self.named(path);
                if self.read(&file).await?.is_none() {
                    return Err(format!(
                        "cannot delete {}: no such file",
                        file.name.display()
                    ));
                }
                self.remove(file).await;
                Ok(format!("D {path}"))
            }
            Section::Update {
                path,
                move_to,
                hunks,
            } => {
                let file = self.named(path);
                let source = self.read(&file).await?.ok_or_else(|| {
                    format!("cannot update {}: no such file", file.name.display())
                })?;
                let updated = apply(&source, hunks)
                    .map_err(|e| format!("cannot update {}: {e}", file.name.display()))?;
                let Some(move_to) = move_to else {
                    self.set(file, Some(updated));
                    return Ok(format!("M {path}"));
                };
                let dest = self.named(move_to);
                // By where the plan keeps each, so a link planned gone
                // and the file it named are two places to move between.
                if self.key(&dest) != self.key(&file) {
                    if self.read(&dest).await?.is_some() {
                        return Err(format!(
                            "cannot move {} to {}: it already exists",
                            file.name.display(),
                            dest.name.display()
                        ));
                    }
                    self.remove(file).await;
                }
                self.set(dest, Some(updated));
                Ok(format!("M {move_to}"))
            }
        }
    }

    /// A file as the sections so far leave it; `None` if it does not exist.
    async fn read(&mut self, file: &Named) -> Result<Option<String>, String> {
        if let Some(planned) = self.files.get(&self.key(file)) {
            return Ok(planned.content.clone());
        }
        match tokio::fs::read_to_string(&file.identity).await {
            Ok(content) => {
                self.on_disk.insert(file.identity.clone());
                Ok(Some(content))
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("cannot read {}: {e}", file.name.display())),
        }
    }

    /// The files the patch leaves in place, added, changed or moved to, by
    /// the name the patch used.
    fn changed(&self) -> Vec<PathBuf> {
        let kept = self.files.values().filter(|file| file.content.is_some());
        kept.map(|file| file.name.clone()).collect()
    }

    /// Deletes first, so a file may give way to a directory of its name
    /// and a symlink to a file of its own. A write goes through a symlink to
    /// the file it names; a link to delete was keyed on itself by
    /// [`Plan::remove`].
    async fn write(self) -> Result<(), String> {
        let deletes = self.files.iter().filter(|(key, file)| {
            file.unlink || (file.content.is_none() && self.on_disk.contains(*key))
        });
        let writes = self.files.iter().filter(|(_, file)| file.content.is_some());
        let operations: Vec<_> = (deletes.map(|(key, file)| ("delete", key, file)))
            .chain(writes.map(|(key, file)| ("write", key, file)))
            .collect();
        let mut completed = Vec::new();
        for (index, (verb, key, file)) in operations.iter().enumerate() {
            let result = match (*verb, &file.content) {
                ("write", Some(content)) => write_atomic(key, content.clone().into_bytes()).await,
                _ => tokio::fs::remove_file(key).await,
            };
            let path = &file.name;
            if let Err(e) = result {
                let pending: Vec<_> = operations[index + 1..]
                    .iter()
                    .map(|(verb, _, file)| format!("{verb} {}", file.name.display()))
                    .collect();
                // A write is atomic unless it had to go in place, whose
                // error already says the file may be partly written.
                let left = if post_write::partly_written(&e) {
                    ""
                } else {
                    "\nThe failing file is as it was."
                };
                return Err(format!(
                    "cannot {verb} {}: {e}\nCompleted operations: {}\nUnattempted operations: {}{left}",
                    path.display(),
                    if completed.is_empty() {
                        "none".into()
                    } else {
                        completed.join(", ")
                    },
                    if pending.is_empty() {
                        "none".into()
                    } else {
                        pending.join(", ")
                    },
                ));
            }
            completed.push(format!("{verb} {}", path.display()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bom::BOM;

    async fn patch(dir: &Path, patch: &str) -> ToolResult {
        let ctx = ToolContext::new(dir.to_path_buf());
        ApplyPatch::new(PostWrite::off())
            .call(json!({ "patchText": patch }), &ctx)
            .await
    }

    fn file(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, content).expect("write");
        path
    }

    fn contents(path: &Path) -> String {
        std::fs::read_to_string(path).expect("read back")
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_patch_that_names_one_file_two_ways_sees_one_file() {
        let dir = tempfile::tempdir().unwrap();
        let real = file(dir.path(), "real.txt", "one\n");
        let alias = dir.path().join("alias.txt");
        std::os::unix::fs::symlink("real.txt", &alias).unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();

        // The second update reads what the first planned through the link,
        // and the delete sees the file the add planned under another name.
        let out = patch(
            dir.path(),
            "*** Begin Patch
*** Update File: real.txt
@@
-one
+two
*** Update File: alias.txt
@@
-two
+three
*** Add File: new.txt
+new
*** Delete File: sub/../new.txt
*** End Patch",
        )
        .await
        .expect("one file under two names");

        assert!(out.contains("M alias.txt"), "{out}");
        assert_eq!(contents(&real), "three\n");
        let link = std::fs::symlink_metadata(&alias).unwrap();
        assert!(link.file_type().is_symlink(), "the link stays a link");
        assert!(!dir.path().join("new.txt").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_delete_or_move_through_a_symlink_takes_the_link() {
        let dir = tempfile::tempdir().unwrap();
        let kept = file(dir.path(), "kept.txt", "kept\n");
        let source = file(dir.path(), "source.txt", "one\n");
        let gone = dir.path().join("gone.txt");
        let moving = dir.path().join("moving.txt");
        std::os::unix::fs::symlink("kept.txt", &gone).unwrap();
        std::os::unix::fs::symlink("source.txt", &moving).unwrap();

        patch(
            dir.path(),
            "*** Begin Patch
*** Delete File: gone.txt
*** Update File: moving.txt
*** Move to: moved.txt
@@
-one
+two
*** End Patch",
        )
        .await
        .expect("links deleted and moved");

        assert!(std::fs::symlink_metadata(&gone).is_err(), "the link went");
        assert_eq!(contents(&kept), "kept\n", "the file it named stays");
        assert!(std::fs::symlink_metadata(&moving).is_err());
        assert_eq!(contents(&source), "one\n");
        assert_eq!(contents(&dir.path().join("moved.txt")), "two\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_link_planned_gone_stays_gone_until_added_as_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = file(dir.path(), "target.txt", "target\n");
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink("target.txt", &link).unwrap();

        // Gone, a later update finds no file, and the patch changes nothing.
        let err = patch(
            dir.path(),
            "*** Begin Patch
*** Delete File: link.txt
*** Update File: link.txt
@@
-target
+changed
*** End Patch",
        )
        .await
        .expect_err("the link was planned gone");
        assert!(err.contains("no such file"), "{err}");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(contents(&target), "target\n");

        // Added again, it is a file of its own and the target is untouched.
        patch(
            dir.path(),
            "*** Begin Patch
*** Delete File: link.txt
*** Add File: link.txt
+own
*** End Patch",
        )
        .await
        .expect("the link replaced by a file");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_file()
        );
        assert_eq!(contents(&link), "own\n");
        assert_eq!(contents(&target), "target\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_link_deleted_while_a_patch_waits_never_costs_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = file(dir.path(), "target.txt", "target\n");
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink("target.txt", &link).unwrap();
        let file_identity = post_write::identity(&target);
        let own = post_write::identity(dir.path()).join("link.txt");
        let post = PostWrite::off();
        // Another patch holds both while this one resolves and waits.
        let held = post.lock_identities(vec![file_identity.clone(), own]).await;
        let tool = ApplyPatch::new(post.clone());
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let waiting = tokio::spawn(async move {
            let text = "*** Begin Patch\n*** Delete File: link.txt\n*** End Patch";
            tool.call(json!({ "patchText": text }), &ctx).await
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while post_write::lock_holders(&file_identity) < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "the patch never waited"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        // The other patch deletes the link, then lets go.
        std::fs::remove_file(&link).unwrap();
        drop(held);

        let err = waiting.await.unwrap().expect_err("the link is gone");
        assert!(err.contains("no such file"), "{err}");
        assert_eq!(contents(&target), "target\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_file_moved_onto_its_deleted_link_leaves_its_old_place() {
        let dir = tempfile::tempdir().unwrap();
        let target = file(dir.path(), "target.txt", "target\n");
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink("target.txt", &link).unwrap();

        patch(
            dir.path(),
            "*** Begin Patch
*** Delete File: link.txt
*** Update File: target.txt
*** Move to: link.txt
@@
-target
+moved
*** End Patch",
        )
        .await
        .expect("a move onto the link's place");

        assert!(!target.exists(), "a move, not a copy");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_file()
        );
        assert_eq!(contents(&link), "moved\n");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_partly_written_file_is_never_said_to_be_as_it_was() {
        if post_write::may_write_dev() {
            // The write would rename over /dev/full instead of failing.
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let full = PathBuf::from("/dev/full");
        let mut plan = Plan::new(dir.path(), HashMap::new());
        let planned = Planned {
            name: full.clone(),
            content: Some("x".into()),
            unlink: false,
        };
        plan.files.insert(full, planned);
        let err = plan.write().await.unwrap_err();
        assert!(err.contains("the file may be partly written"), "{err}");
        assert!(!err.contains("as it was"), "{err}");
    }

    #[tokio::test]
    async fn write_failure_reports_completed_and_unattempted_operations() {
        let dir = tempfile::tempdir().unwrap();
        let deleted = file(dir.path(), "deleted", "old");
        let first = dir.path().join("a");
        let blocker = file(dir.path(), "blocker", "not a directory");
        let failing = blocker.join("child");
        let last = dir.path().join("z");
        let mut plan = Plan::new(dir.path(), HashMap::new());
        for (path, content) in [
            (&deleted, None),
            (&first, Some("written")),
            (&failing, Some("cannot write")),
            (&last, Some("not reached")),
        ] {
            let planned = Planned {
                name: path.clone(),
                content: content.map(String::from),
                unlink: false,
            };
            plan.files.insert(path.clone(), planned);
        }
        plan.on_disk.insert(deleted.clone());
        let err = plan.write().await.unwrap_err();
        assert!(
            err.contains(&format!("cannot write {}", failing.display())),
            "{err}"
        );
        assert!(
            err.contains(&format!("delete {}", deleted.display())),
            "{err}"
        );
        assert!(err.contains(&format!("write {}", first.display())), "{err}");
        assert!(err.contains(&format!("write {}", last.display())), "{err}");
        assert!(err.contains("The failing file is as it was."), "{err}");
        assert!(!deleted.exists());
        assert_eq!(contents(&first), "written");
        assert!(!last.exists());
    }

    #[tokio::test]
    async fn a_write_failure_through_the_tool_reports_the_receipt_and_skips_formatting() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ApplyPatch::new(PostWrite::with_formatter(
            "upper",
            &[
                "sh",
                "-c",
                "tr a-z A-Z < $FILE > $FILE.tmp && mv $FILE.tmp $FILE",
            ],
        ));
        let ctx = ToolContext::new(dir.path().to_path_buf());
        // Adding `x.txt/child.txt` is valid while `x.txt` is missing, but
        // writing `x.txt` first turns it into the child's parent file.
        let err = tool
            .call(
                json!({ "patchText": "*** Begin Patch
*** Add File: x.txt
+one
*** Add File: x.txt/child.txt
+two
*** Add File: z.txt
+three
*** End Patch" }),
                &ctx,
            )
            .await
            .expect_err("x.txt is a file before its child is written");

        let written = dir.path().join("x.txt");
        let failing = written.join("child.txt");
        let unattempted = dir.path().join("z.txt");
        assert!(
            err.contains(&format!("cannot write {}", failing.display())),
            "{err}"
        );
        assert!(
            err.contains(&format!(
                "Completed operations: write {}",
                written.display()
            )),
            "{err}"
        );
        assert!(
            err.contains(&format!(
                "Unattempted operations: write {}",
                unattempted.display()
            )),
            "{err}"
        );
        assert!(err.contains("The failing file is as it was."), "{err}");
        assert!(!err.contains("Formatted with"), "{err}");
        assert_eq!(contents(&written), "one\n");
        assert!(!unattempted.exists());
    }

    #[tokio::test]
    async fn a_move_locks_its_source_and_destination_under_canonical_names() {
        let dir = tempfile::tempdir().unwrap();
        let from = file(dir.path(), "a", "a\n");
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let to = dir.path().join("b");
        let post = PostWrite::off();
        let tool = ApplyPatch::new(post.clone());
        let ctx = ToolContext::new(dir.path().to_path_buf());
        // Both spellings are aliases of the paths the locks are named by.
        let text = json!({ "patchText": "*** Begin Patch
*** Update File: sub/../a
*** Move to: ./b
@@
-a
+b
*** End Patch" });

        let held = post.lock(std::slice::from_ref(&from)).await;
        {
            let call = tool.call(text.clone(), &ctx);
            tokio::pin!(call);
            assert!(futures::poll!(&mut call).is_pending(), "source is locked");
        }
        drop(held);

        let held = post.lock(std::slice::from_ref(&to)).await;
        {
            let call = tool.call(text.clone(), &ctx);
            tokio::pin!(call);
            assert!(
                futures::poll!(&mut call).is_pending(),
                "destination is locked"
            );
        }
        drop(held);

        let out = tool.call(text, &ctx).await.expect("patch");
        assert_eq!(out, "Success. Updated the following files:\nM ./b");
        assert!(!from.exists());
        assert_eq!(contents(&to), "b\n");
    }

    #[tokio::test]
    async fn overlapping_patches_in_opposite_order_append_without_losing_a_write() {
        let dir = tempfile::tempdir().unwrap();
        let a = file(dir.path(), "a", "a\n");
        let b = file(dir.path(), "b", "b\n");
        let tool = ApplyPatch::new(PostWrite::off());
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let one = tool.call(
            json!({ "patchText": "*** Begin Patch
*** Update File: b
@@
+one-b
*** Update File: a
@@
+one-a
*** End Patch" }),
            &ctx,
        );
        let two = tool.call(
            json!({ "patchText": "*** Begin Patch
*** Update File: a
@@
+two-a
*** Update File: b
@@
+two-b
*** End Patch" }),
            &ctx,
        );

        let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(one, two)
        })
        .await
        .expect("patches with overlapping paths do not deadlock");
        first.expect("first patch");
        second.expect("second patch");
        for (path, lines) in [(&a, ["one-a", "two-a"]), (&b, ["one-b", "two-b"])] {
            let content = contents(path);
            for line in lines {
                assert!(content.contains(line), "{}: {content}", path.display());
            }
        }
    }

    #[tokio::test]
    async fn plan_mode_refuses_the_whole_patch_when_one_file_is_not_the_plan() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plan = dir.path().join("plan.md");
        let ctx = ToolContext {
            writable: nth_protocol::Writable::plan(plan.clone()),
            ..ToolContext::new(dir.path().to_path_buf())
        };
        let text = "*** Begin Patch\n*** Add File: plan.md\n+# Plan\n*** Add File: b.txt\n+b\n*** End Patch";

        let out = ApplyPatch::new(PostWrite::off())
            .call(json!({ "patchText": text }), &ctx)
            .await;

        let err = out.expect_err("refused");
        assert!(err.contains("plan mode is active"), "{err}");
        assert!(err.ends_with("No files were changed."), "{err}");
        assert!(!plan.exists());
        assert!(!dir.path().join("b.txt").exists());
    }

    #[tokio::test]
    async fn adds_a_file_and_its_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = patch(
            dir.path(),
            "*** Begin Patch\n*** Add File: a/b.txt\n+one\n+\n+two\n*** End Patch",
        )
        .await
        .expect("patch");
        assert_eq!(out, "Success. Updated the following files:\nA a/b.txt");
        assert_eq!(contents(&dir.path().join("a/b.txt")), "one\n\ntwo\n");
    }

    #[tokio::test]
    async fn updates_with_several_hunks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(
            dir.path(),
            "a.rs",
            "fn a() {\n    1\n}\n\nfn b() {\n    2\n}\n\nfn c() {\n    3\n}\n",
        );
        let out = patch(
            dir.path(),
            "\
*** Begin Patch
*** Update File: a.rs
@@ fn a() {
-    1
+    10
 }
@@ fn c() {
-    3
+    30
+    31
 }
*** End Patch",
        )
        .await
        .expect("patch");
        assert_eq!(out, "Success. Updated the following files:\nM a.rs");
        assert_eq!(
            contents(&path),
            "fn a() {\n    10\n}\n\nfn b() {\n    2\n}\n\nfn c() {\n    30\n    31\n}\n"
        );
    }

    #[tokio::test]
    async fn a_second_hunk_under_the_same_at_line_says_where_it_looked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "a.rs", "fn a() {\n    1\n    2\n}\n");
        let err = patch(
            dir.path(),
            "\
*** Begin Patch
*** Update File: a.rs
@@ fn a() {
-    1
+    10
@@ fn a() {
-    2
+    20
*** End Patch",
        )
        .await
        .expect_err("the anchor is behind the cursor");
        assert!(
            err.contains("hunk 2: the @@ line `fn a() {` is not in the file after hunk 1"),
            "{err}"
        );
        assert!(err.contains("in one hunk"), "{err}");
        assert_eq!(contents(&path), "fn a() {\n    1\n    2\n}\n");
    }

    #[tokio::test]
    async fn the_at_line_picks_between_repeated_blocks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(
            dir.path(),
            "a.py",
            "def f():\n    pass\n\ndef g():\n    pass\n",
        );
        let ambiguous =
            "*** Begin Patch\n*** Update File: a.py\n@@\n-    pass\n+    return 1\n*** End Patch";
        let err = patch(dir.path(), ambiguous).await.expect_err("ambiguous");
        assert!(err.contains("hunk 1 matches 2 places"), "{err}");

        patch(
            dir.path(),
            "*** Begin Patch\n*** Update File: a.py\n@@ def g():\n-    pass\n+    return 1\n*** End Patch",
        )
        .await
        .expect("patch");
        assert_eq!(
            contents(&path),
            "def f():\n    pass\n\ndef g():\n    return 1\n"
        );
    }

    #[tokio::test]
    async fn later_hunks_search_after_earlier_ones() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "a", "x\ny\nx\n");
        patch(
            dir.path(),
            "*** Begin Patch\n*** Update File: a\n@@\n-x\n+1\n y\n@@\n-x\n+2\n*** End Patch",
        )
        .await
        .expect("patch");
        assert_eq!(contents(&path), "1\ny\n2\n");
    }

    #[tokio::test]
    async fn matches_whole_lines_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "a", "max = 1\n  x = 1\n");
        patch(
            dir.path(),
            "*** Begin Patch\n*** Update File: a\n@@\n-x = 1\n+  x = 2\n*** End Patch",
        )
        .await
        .expect("patch");
        assert_eq!(contents(&path), "max = 1\n  x = 2\n");
    }

    #[tokio::test]
    async fn pure_additions_go_after_the_at_line_or_at_the_end() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "a", "head\nmid\ntail");
        patch(
            dir.path(),
            "*** Begin Patch\n*** Update File: a\n@@\n+end\n@@ head\n+after head\n*** End Patch",
        )
        .await
        .expect("patch");
        assert_eq!(contents(&path), "head\nafter head\nmid\ntail\nend\n");
    }

    #[tokio::test]
    async fn pure_addition_with_end_of_file_goes_at_the_end() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "a", "head\ntail\n");
        patch(
            dir.path(),
            "*** Begin Patch\n*** Update File: a\n@@ head\n+end\n*** End of File\n*** End Patch",
        )
        .await
        .expect("patch");
        assert_eq!(contents(&path), "head\ntail\nend\n");
    }

    #[tokio::test]
    async fn eof_addition_requires_its_anchor_and_preserves_crlf() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "a", "café\r\ntail\r\n");
        let missing =
            "*** Begin Patch\n*** Update File: a\n@@ missing\n+fin\n*** End of File\n*** End Patch";
        assert!(patch(dir.path(), missing).await.is_err());
        assert_eq!(contents(&path), "café\r\ntail\r\n");
        let valid =
            "*** Begin Patch\n*** Update File: a\n@@ café\n+fin\n*** End of File\n*** End Patch";
        patch(dir.path(), valid).await.expect("patch");
        assert_eq!(contents(&path), "café\r\ntail\r\nfin\r\n");
    }

    #[tokio::test]
    async fn end_of_file_anchors_to_the_last_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "a", "}\nfn x() {\n}\n");
        patch(
            dir.path(),
            "*** Begin Patch\n*** Update File: a\n@@\n-}\n+} // x\n*** End of File\n*** End Patch",
        )
        .await
        .expect("patch");
        assert_eq!(contents(&path), "}\nfn x() {\n} // x\n");
    }

    #[tokio::test]
    async fn deletes_a_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "old.txt", "bye\n");
        let out = patch(
            dir.path(),
            "*** Begin Patch\n*** Delete File: old.txt\n*** End Patch",
        )
        .await
        .expect("patch");
        assert_eq!(out, "Success. Updated the following files:\nD old.txt");
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn moves_and_updates_a_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let from = file(dir.path(), "app.py", "def greet():\n    print(\"Hi\")\n");
        let out = patch(
            dir.path(),
            "\
*** Begin Patch
*** Update File: app.py
*** Move to: src/main.py
@@ def greet():
-    print(\"Hi\")
+    print(\"Hello, world!\")
*** End Patch",
        )
        .await
        .expect("patch");
        assert_eq!(out, "Success. Updated the following files:\nM src/main.py");
        assert!(!from.exists());
        assert_eq!(
            contents(&dir.path().join("src/main.py")),
            "def greet():\n    print(\"Hello, world!\")\n"
        );
    }

    #[tokio::test]
    async fn moving_onto_an_existing_file_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let from = file(dir.path(), "a", "a\n");
        let to = file(dir.path(), "b", "b\n");
        let err = patch(
            dir.path(),
            "*** Begin Patch\n*** Update File: a\n*** Move to: b\n*** End Patch",
        )
        .await
        .expect_err("exists");
        assert!(err.contains("already exists"), "{err}");
        assert_eq!(
            (contents(&from), contents(&to)),
            ("a\n".into(), "b\n".into())
        );
    }

    #[tokio::test]
    async fn falls_back_to_fuzzy_whitespace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(
            dir.path(),
            "a.rs",
            "fn f() {\n\tlet x = 1;  \n\tlet y = 2;\n}\n",
        );
        patch(
            dir.path(),
            "*** Begin Patch\n*** Update File: a.rs\n@@ fn f() {\n-    let x = 1;\n+\tlet x = 10;\n     let y = 2;\n*** End Patch",
        )
        .await
        .expect("patch");
        assert_eq!(
            contents(&path),
            "fn f() {\n\tlet x = 10;\n    let y = 2;\n}\n"
        );
    }

    #[tokio::test]
    async fn drops_a_trailing_blank_context_line_the_file_lacks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "a", "a\nb\n");
        patch(
            dir.path(),
            "*** Begin Patch\n*** Update File: a\n@@\n-b\n+c\n\n*** Add File: z\n+z\n*** End Patch",
        )
        .await
        .expect("patch");
        assert_eq!(contents(&path), "a\nc\n");
    }

    #[tokio::test]
    async fn a_failing_hunk_changes_no_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = file(dir.path(), "a", "one\n");
        let b = file(dir.path(), "b", "two\n");
        let gone = file(dir.path(), "gone", "still here\n");
        let err = patch(
            dir.path(),
            "\
*** Begin Patch
*** Add File: new
+new
*** Update File: a
@@
-one
+1
*** Delete File: gone
*** Update File: b
@@
 two
-three
+3
*** End Patch",
        )
        .await
        .expect_err("hunk fails");
        assert!(
            err.contains(&format!("cannot update {}", b.display())),
            "{err}"
        );
        assert!(err.contains("hunk 1 not found"), "{err}");
        assert!(err.contains("No files were changed"), "{err}");
        assert_eq!(contents(&a), "one\n");
        assert_eq!(contents(&b), "two\n");
        assert_eq!(contents(&gone), "still here\n");
        assert!(!dir.path().join("new").exists());
    }

    #[tokio::test]
    async fn missing_and_existing_targets_are_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "a", "keep\n");
        let cases = [
            ("*** Add File: a\n+x", "already exists"),
            ("*** Update File: nope\n@@\n-x\n+y", "no such file"),
            ("*** Delete File: nope", "no such file"),
        ];
        for (section, want) in cases {
            let err = patch(
                dir.path(),
                &format!("*** Begin Patch\n{section}\n*** End Patch"),
            )
            .await
            .expect_err(section);
            assert!(err.contains(want), "{section}: {err}");
        }
        assert_eq!(contents(&path), "keep\n");
    }

    #[tokio::test]
    async fn a_malformed_patch_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = patch(dir.path(), "*** Add File: a\n+x")
            .await
            .expect_err("malformed");
        assert!(err.starts_with("invalid patch:"), "{err}");
        assert!(
            patch(dir.path(), "*** Begin Patch\n*** End Patch")
                .await
                .is_err()
        );
        let ctx = ToolContext::new(dir.path().to_path_buf());
        assert!(
            ApplyPatch::new(PostWrite::off())
                .call(json!({}), &ctx)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn sections_see_what_earlier_ones_planned() {
        let dir = tempfile::tempdir().expect("tempdir");
        file(dir.path(), "old", "x\n");
        patch(
            dir.path(),
            "\
*** Begin Patch
*** Add File: a
+one
*** Update File: a
@@
-one
+two
*** Delete File: old
*** Add File: old
+fresh
*** End Patch",
        )
        .await
        .expect("patch");
        assert_eq!(contents(&dir.path().join("a")), "two\n");
        assert_eq!(contents(&dir.path().join("old")), "fresh\n");
    }

    #[tokio::test]
    async fn keeps_crlf_and_a_bom() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "a.cs", &format!("{BOM}a\r\nb\r\nc\r\n"));
        patch(
            dir.path(),
            "*** Begin Patch\n*** Update File: a.cs\n@@\n a\n-b\n+x\n+y\n*** End Patch",
        )
        .await
        .expect("patch");
        assert_eq!(contents(&path), format!("{BOM}a\r\nx\r\ny\r\nc\r\n"));
    }

    #[tokio::test]
    async fn formats_each_file_it_leaves_and_names_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        file(dir.path(), "a.txt", "a\n");
        file(dir.path(), "gone.txt", "x\n");
        file(dir.path(), "from.txt", "m\n");
        let tool = ApplyPatch::new(PostWrite::with_formatter(
            "upper",
            &[
                "sh",
                "-c",
                "tr a-z A-Z < $FILE > $FILE.tmp && mv $FILE.tmp $FILE",
            ],
        ));
        let ctx = ToolContext::new(dir.path().to_path_buf());

        let out = tool
            .call(
                json!({ "patchText": "*** Begin Patch
*** Update File: a.txt
@@
-a
+b
*** Delete File: gone.txt
*** Add File: sub/new.txt
+n
*** Update File: from.txt
*** Move to: to.txt
@@
-m
+t
*** End Patch" }),
                &ctx,
            )
            .await
            .expect("patch");

        assert_eq!(
            out,
            "Success. Updated the following files:\nM a.txt\nD gone.txt\nA sub/new.txt\nM to.txt\n\n\
             a.txt: Formatted with upper.\n\
             sub/new.txt: Formatted with upper.\n\
             to.txt: Formatted with upper."
        );
        assert_eq!(contents(&dir.path().join("a.txt")), "B\n");
        assert_eq!(contents(&dir.path().join("sub/new.txt")), "N\n");
        assert_eq!(contents(&dir.path().join("to.txt")), "T\n");
        assert!(!dir.path().join("gone.txt").exists());
        assert!(!dir.path().join("from.txt").exists());
    }

    #[tokio::test]
    async fn bom_survives_the_formatter() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = file(dir.path(), "a.txt", &format!("{BOM}old\n"));
        let tool = ApplyPatch::new(PostWrite::with_formatter(
            "rewrite",
            &["sh", "-c", "printf formatted > $FILE"],
        ));
        let ctx = ToolContext::new(dir.path().to_path_buf());

        tool.call(
            json!({ "patchText": "*** Begin Patch\n*** Update File: a.txt\n@@\n-old\n+new\n*** End Patch" }),
            &ctx,
        )
        .await
        .expect("patch");

        assert_eq!(contents(&path), format!("{BOM}formatted"));
    }

    /// The whole path with a real rust-analyzer: run it by hand with
    /// `cargo test -p nth-tools -- --ignored`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs rust-analyzer on PATH"]
    async fn reports_rust_analyzer_errors_per_changed_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        crate::post_write::broken_crate(dir.path());
        let tool = ApplyPatch::new(PostWrite::lsp_only());
        let ctx = ToolContext::new(dir.path().to_path_buf());

        let out = tool
            .call(
                json!({ "patchText": "*** Begin Patch\n*** Update File: src/main.rs\n@@\n-    let x: u32 = 1;\n+    let x: u32 = \"one\";\n*** End Patch" }),
                &ctx,
            )
            .await
            .expect("patch");

        println!("{out}");
        let main = dir.path().join("src/main.rs");
        assert!(
            out.starts_with(&format!(
                "Success. Updated the following files:\nM src/main.rs\n\n\
                 LSP errors detected in src/main.rs, please fix:\n<diagnostics file=\"{}\">\nERROR [4:",
                main.display()
            )),
            "{out}"
        );
        assert!(!out.contains("other.rs"), "{out}");
    }
}
