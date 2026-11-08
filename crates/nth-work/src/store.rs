//! The work plan on disk: `<root>/.nth/work/<session>/`, a numbered folder
//! per cycle (`01-foundation/`) and one markdown file per item in it, its
//! state in the frontmatter and its brief, todo and result in the body.
//! Only this crate writes item files; it reads whatever is there,
//! hand-edited or not.

use std::{
    fs, io,
    path::{Path, PathBuf},
    time::SystemTime,
};

use crate::{
    Cycle, Item, ItemRef, Plan, Status, cycle_name,
    format::{overview, parse, render},
};

/// A session's work directory.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The whole plan; `None` when the session never wrote one.
    pub async fn load(&self) -> io::Result<Option<Plan>> {
        let dir = self.dir.clone();
        // One hop to the blocking pool for the whole walk, not one per file.
        tokio::task::spawn_blocking(move || load(&dir))
            .await
            .map_err(io::Error::other)?
    }

    /// One item, as `load` reads it; `None` when there is no such file.
    pub async fn read(&self, at: &ItemRef) -> io::Result<Option<Item>> {
        let path = at.path(&self.dir);
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let modified = tokio::fs::metadata(&path)
            .await
            .and_then(|m| m.modified())
            .ok();
        Ok(Some(item(at.clone(), &text, modified)))
    }

    /// Writes `item`, its cycle's folder too if it is new. Written aside
    /// and renamed over, so a reader never sees half a file.
    pub async fn write(&self, item: &Item) -> io::Result<()> {
        let path = item.at.path(&self.dir);
        if let Some(folder) = path.parent() {
            tokio::fs::create_dir_all(folder).await?;
        }
        // Its own name per write: a write cancelled mid-way still lands,
        // and must not land in the file another write is about to rename.
        static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = path.with_extension(format!("md.{}.{n}.tmp", std::process::id()));
        tokio::fs::write(&tmp, render(item)).await?;
        tokio::fs::rename(&tmp, &path).await
    }
}

fn load(dir: &Path) -> io::Result<Option<Plan>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut plan = Plan::default();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !entry.file_type()?.is_dir() {
            continue;
        }
        // The tools find a cycle's items by `cycle_name`, so a folder named
        // otherwise (`1-a`) would hold items nobody can reach.
        let Some((number, slug)) = parse_cycle(&name).filter(|(n, s)| cycle_name(*n, s) == name)
        else {
            plan.ignored.push(name);
            continue;
        };
        let mut items = Vec::new();
        for file in fs::read_dir(entry.path())? {
            let file = file?;
            let file_name = file.file_name().to_string_lossy().into_owned();
            let Some(slug) = file_name.strip_suffix(".md") else {
                continue;
            };
            let at = ItemRef {
                cycle: name.clone(),
                item: slug.to_string(),
            };
            let modified = file.metadata().and_then(|m| m.modified()).ok();
            let item = match fs::read_to_string(file.path()) {
                Ok(text) => item(at, &text, modified),
                Err(e) => broken(at, e.to_string()),
            };
            items.push(item);
        }
        items.sort_by(|a, b| a.at.item.cmp(&b.at.item));
        plan.cycles.push(Cycle {
            number,
            slug: slug.to_string(),
            items,
        });
    }
    plan.cycles
        .sort_by(|a, b| (a.number, &a.slug).cmp(&(b.number, &b.slug)));
    // One folder a number: the tools reach the first only, and the rest
    // would hold every later cycle up for good.
    let mut seen = std::collections::BTreeSet::new();
    let (cycles, doubles): (Vec<_>, Vec<_>) = std::mem::take(&mut plan.cycles)
        .into_iter()
        .partition(|cycle| seen.insert(cycle.number));
    plan.cycles = cycles;
    plan.ignored.extend(doubles.iter().map(Cycle::name));
    plan.ignored.sort();
    match fs::read_to_string(dir.join("plan.md")) {
        Ok(text) => (plan.title, plan.goal) = overview(&text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    Ok(Some(plan))
}

/// `01-foundation` as its number and slug.
fn parse_cycle(name: &str) -> Option<(u32, &str)> {
    let (number, slug) = name.split_once('-')?;
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    crate::slug(slug).ok()?;
    Some((number.parse().ok()?, slug))
}

fn item(at: ItemRef, text: &str, modified: Option<SystemTime>) -> Item {
    match parse(at.clone(), text) {
        Ok(item) => Item { modified, ..item },
        Err(why) => Item {
            modified,
            ..broken(at, why)
        },
    }
}

fn broken(at: ItemRef, why: String) -> Item {
    let title = at.item.clone();
    Item {
        status: Status::Broken(why),
        ..Item::pending(at, &title, "", Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(cycle: &str, item: &str) -> ItemRef {
        ItemRef {
            cycle: cycle.into(),
            item: item.into(),
        }
    }

    #[tokio::test]
    async fn the_plan_loads_by_cycle_number_and_keeps_broken_items() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("work"));
        assert_eq!(store.load().await.unwrap(), None, "no plan yet");

        let b = Item::pending(at("02-b", "late"), "Late", "", Vec::new());
        let a = Item {
            status: Status::Done,
            ..Item::pending(at("01-a", "first"), "First", "", Vec::new())
        };
        store.write(&b).await.unwrap();
        store.write(&a).await.unwrap();
        std::fs::write(store.dir().join("01-a/notes.txt"), "stray").unwrap();
        std::fs::write(store.dir().join("01-a/bad.md"), "garbage").unwrap();
        std::fs::create_dir(store.dir().join("misc")).unwrap();
        std::fs::write(store.dir().join("plan.md"), "# Retry\n\nAdd retries.\n").unwrap();

        let plan = store.load().await.unwrap().unwrap();

        assert_eq!(plan.title.as_deref(), Some("Retry"));
        assert_eq!(plan.goal.as_deref(), Some("Add retries."));
        let names: Vec<_> = plan.cycles.iter().map(Cycle::name).collect();
        assert_eq!(names, ["01-a", "02-b"]);
        let first: Vec<_> = plan.cycles[0]
            .items
            .iter()
            .map(|i| i.at.item.as_str())
            .collect();
        assert_eq!(first, ["bad", "first"]);
        assert!(matches!(plan.cycles[0].items[0].status, Status::Broken(_)));
        assert!(plan.cycles[0].items[1].modified.is_some());
        assert_eq!(
            plan.first_open_cycle().map(Cycle::name).as_deref(),
            Some("01-a"),
            "a broken item keeps its cycle open"
        );
        std::fs::remove_file(store.dir().join("01-a/bad.md")).unwrap();
        let plan = store.load().await.unwrap().unwrap();
        assert_eq!(
            plan.first_open_cycle().map(Cycle::name).as_deref(),
            Some("02-b")
        );
        assert_eq!(plan.ignored, ["misc"]);
        std::fs::create_dir(store.dir().join("1-x")).unwrap();
        std::fs::create_dir(store.dir().join("02-a")).unwrap();
        let reloaded = store.load().await.unwrap().unwrap();
        assert_eq!(
            reloaded.ignored,
            ["02-b", "1-x", "misc"],
            "02-a sorts first"
        );
        assert_eq!(
            store
                .read(&at("02-b", "late"))
                .await
                .unwrap()
                .map(|i| i.title),
            Some("Late".into())
        );
        assert_eq!(store.read(&at("02-b", "gone")).await.unwrap(), None);
    }
}
