//! What earlier scans measured, so the app can show what keeps growing.
//!
//! Every finished scan is written down as a snapshot: the size of each entry
//! of at least [`THRESHOLD`] beneath the scanned root, keyed by absolute path.
//! Crumbs are renumbered by every scan, so a path is the only key that
//! survives one. Leaving small entries out keeps a snapshot of a whole disk
//! to about a megabyte. An entry a covering snapshot does
//! not list was smaller than the threshold then, so treating it as empty
//! overstates its growth by less than the threshold.
//!
//! A snapshot is a plain text file. There is one per root, scan settings and
//! UTC day, and the day's first scan is the one kept. Later scans that day
//! leave it alone, so the rescan after every removal does not pile files up,
//! and the scan on screen never overwrites the baseline it is compared with.

use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use rustc_hash::{FxHashMap, FxHasher};

use crate::scan::ScanOptions;
use crate::tree::Node;

/// Entries smaller than this are not written down.
pub const THRESHOLD: u64 = 16 * 1024 * 1024;

/// Growth below this is not worth a line in the list.
pub const MIN_GROWTH: u64 = 64 * 1024 * 1024;

/// Seconds in a day.
const DAY: i64 = 86_400;

/// Snapshots older than this are deleted when a new one is written.
const KEEP_DAYS: i64 = 90;

/// A baseline must be at least this much older than now. Without it, the
/// snapshot of the scan on screen would be its own baseline and show no
/// growth anywhere.
const MIN_GAP: i64 = 60 * 60;

const MAGIC: &str = "disktree drift 1";
const EXTENSION: &str = "drift";

/// One earlier scan's sizes.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// Unix seconds when it was written.
    pub taken: i64,
    pub root: PathBuf,
    pub sizes: FxHashMap<PathBuf, u64>,
}

/// How the tree on screen compares with a snapshot.
#[derive(Clone, Debug, Default)]
pub struct Growth {
    /// When the baseline was taken, in Unix seconds.
    pub since: i64,
    /// Change in bytes for every node either scan measured at or above
    /// [`THRESHOLD`]. Nodes both scans found small have no entry.
    deltas: FxHashMap<Vec<usize>, i64>,
    /// Where growth happened, largest first. See [`growth`].
    pub growers: Vec<Grower>,
}

impl Growth {
    /// The change in bytes at `crumbs`, if either scan measured it.
    pub fn delta(&self, crumbs: &[usize]) -> Option<i64> {
        self.deltas.get(crumbs).copied()
    }
}

/// One line of the growth list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grower {
    /// Where it is, from the scanned root.
    pub crumbs: Vec<usize>,
    pub delta: i64,
}

/// What a scan's settings change about its sizes. Snapshots are only
/// compared with scans that measured the same way: a scan without hidden
/// files would otherwise show `~/.cache` as having shrunk to nothing, and
/// one run as root would show every folder only root can read as new growth.
fn settings(options: &ScanOptions) -> String {
    let depth = options
        .max_depth
        .map_or_else(|| "none".to_owned(), |depth| depth.to_string());
    format!(
        "apparent={} links={} hidden={} one-filesystem={} dedup={} \
         depth={} root={}",
        u8::from(options.apparent_size),
        u8::from(options.follow_links),
        u8::from(options.include_hidden),
        u8::from(options.one_filesystem),
        u8::from(options.dedup_hardlinks),
        depth,
        u8::from(crate::access::as_root()),
    )
}

/// Write `tree`, scanned from `root` at `taken`, into `dir` unless that day
/// already has a snapshot of it, and delete snapshots older than
/// [`KEEP_DAYS`].
pub fn record(
    dir: &Path,
    root: &Path,
    tree: &Node,
    options: &ScanOptions,
    taken: i64,
) -> io::Result<()> {
    let Some(root_text) = root.to_str().filter(|text| writable(text)) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the root's path cannot be written on one line",
        ));
    };
    let settings = settings(options);
    fs::create_dir_all(dir)?;
    let mut key = FxHasher::default();
    (root_text, &settings).hash(&mut key);
    let day = taken.div_euclid(DAY);
    let target = dir.join(format!("{day}-{:016x}.{EXTENSION}", key.finish()));
    if target.exists() {
        return prune(dir, day - KEEP_DAYS);
    }

    let mut text = format!(
        "{MAGIC}\ntaken {taken}\nroot {root_text}\nsettings {settings}\n\n"
    );
    let mut path = root.to_path_buf();
    write_entries(tree, &mut path, &mut text);

    // Written beside the target and renamed into place, so a reader never
    // sees half a snapshot.
    let partial = target.with_extension("partial");
    let mut file = fs::File::create(&partial)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    fs::rename(&partial, &target)?;

    prune(dir, day - KEEP_DAYS)
}

/// A path is written only when a line break cannot split it.
fn writable(text: &str) -> bool {
    !text.contains(['\n', '\r'])
}

fn write_entries(node: &Node, path: &mut PathBuf, text: &mut String) {
    if node.bytes < THRESHOLD {
        return;
    }
    if let Some(line) = path.to_str().filter(|line| writable(line)) {
        text.push_str(&node.bytes.to_string());
        text.push('\t');
        text.push_str(line);
        text.push('\n');
    }
    // Children are sorted largest first, so the first small one ends it.
    for child in &node.children {
        if child.bytes < THRESHOLD {
            break;
        }
        path.push(&*child.name);
        write_entries(child, path, text);
        path.pop();
    }
}

/// The UTC day a snapshot's file name says it was taken on.
fn day_of(path: &Path) -> Option<i64> {
    if path.extension()? != EXTENSION {
        return None;
    }
    path.file_name()?.to_str()?.split('-').next()?.parse().ok()
}

fn prune(dir: &Path, before_day: i64) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if day_of(&path).is_some_and(|day| day < before_day) {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

/// What a snapshot's header says.
struct Header {
    taken: i64,
    root: PathBuf,
    settings: String,
}

fn read_header(
    lines: &mut impl Iterator<Item = io::Result<String>>,
) -> Option<Header> {
    if lines.next()?.ok()? != MAGIC {
        return None;
    }
    let mut field = |name: &str| -> Option<String> {
        let line = lines.next()?.ok()?;
        line.strip_prefix(name)?
            .strip_prefix(' ')
            .map(str::to_owned)
    };
    Some(Header {
        taken: field("taken")?.parse().ok()?,
        root: PathBuf::from(field("root")?),
        settings: field("settings")?,
    })
}

/// The snapshot to measure growth of `root` against, for a scan taken at
/// `now` with `options`: the newest one taken at or before `since`, or, while
/// history is shorter than that, the oldest one there is.
///
/// Only snapshots of `root` or a directory above it are used: a narrower one
/// has nothing to say about most of the tree.
pub fn baseline(
    dir: &Path,
    root: &Path,
    options: &ScanOptions,
    since: i64,
    now: i64,
) -> io::Result<Option<Snapshot>> {
    let settings = settings(options);
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };

    let mut newest_before: Option<(i64, PathBuf)> = None;
    let mut oldest: Option<(i64, PathBuf)> = None;
    for entry in entries {
        let path = entry?.path();
        if day_of(&path).is_none() {
            continue;
        }
        let Ok(file) = fs::File::open(&path) else {
            continue;
        };
        let Some(header) = read_header(&mut BufReader::new(file).lines())
        else {
            continue;
        };
        if header.settings != settings
            || !root.starts_with(&header.root)
            || header.taken > now - MIN_GAP
        {
            continue;
        }
        let taken = header.taken;
        if taken <= since
            && newest_before.as_ref().is_none_or(|(t, _)| taken > *t)
        {
            newest_before = Some((taken, path.clone()));
        }
        if oldest.as_ref().is_none_or(|(t, _)| taken < *t) {
            oldest = Some((taken, path));
        }
    }

    let Some((_, path)) = newest_before.or(oldest) else {
        return Ok(None);
    };
    read(&path).map(Some)
}

fn read(path: &Path) -> io::Result<Snapshot> {
    let mut lines = BufReader::new(fs::File::open(path)?).lines();
    let header = read_header(&mut lines).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "not a drift snapshot")
    })?;
    let mut sizes = FxHashMap::default();
    for line in lines {
        let line = line?;
        let Some((bytes, path)) = line.split_once('\t') else {
            continue;
        };
        if let Ok(bytes) = bytes.parse() {
            sizes.insert(PathBuf::from(path), bytes);
        }
    }
    Ok(Snapshot {
        taken: header.taken,
        root: header.root,
        sizes,
    })
}

fn signed(bytes: u64) -> i64 {
    i64::try_from(bytes).unwrap_or(i64::MAX)
}

/// Compare `tree`, scanned from `root`, with `baseline`.
///
/// The list names where growth happened rather than every directory above
/// it: a node is listed when it grew by at least [`MIN_GROWTH`] and no single
/// child accounts for half of that. `~/Library` growing because one cache did
/// lists the cache, not `~/Library`.
pub fn growth(root: &Path, tree: &Node, baseline: &Snapshot) -> Growth {
    let mut growth = Growth {
        since: baseline.taken,
        ..Growth::default()
    };
    let mut path = root.to_path_buf();
    let mut crumbs = Vec::new();
    visit(tree, &mut path, &mut crumbs, baseline, &mut growth);
    growth
        .growers
        .sort_by_key(|grower| std::cmp::Reverse(grower.delta));
    growth
}

/// Record `node`'s change and its children's; returns the change, or `None`
/// when neither scan measured it.
fn visit(
    node: &Node,
    path: &mut PathBuf,
    crumbs: &mut Vec<usize>,
    baseline: &Snapshot,
    growth: &mut Growth,
) -> Option<i64> {
    let before = baseline.sizes.get(path.as_path()).copied();
    if before.is_none() && node.bytes < THRESHOLD {
        // Small in both scans, and so is everything beneath it.
        return None;
    }
    let delta = signed(node.bytes) - signed(before.unwrap_or(0));

    let mut largest_child = i64::MIN;
    for (index, child) in node.children.iter().enumerate() {
        path.push(&*child.name);
        crumbs.push(index);
        if let Some(change) = visit(child, path, crumbs, baseline, growth) {
            largest_child = largest_child.max(change);
        }
        crumbs.pop();
        path.pop();
    }

    if delta >= signed(MIN_GROWTH) && largest_child.saturating_mul(2) < delta {
        growth.growers.push(Grower {
            crumbs: crumbs.clone(),
            delta,
        });
    }
    growth.deltas.insert(crumbs.clone(), delta);
    Some(delta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Metric, NodeKind, aggregate, path_of};

    const MIB: u64 = 1024 * 1024;
    const NOW: i64 = 1_800_000_000;

    fn file(name: &str, bytes: u64) -> Node {
        Node::entry(name, NodeKind::File, bytes)
    }

    fn dir(name: &str, children: Vec<Node>) -> Node {
        let mut node = Node::directory(name);
        node.children = children;
        node
    }

    fn scanned(mut root: Node) -> Node {
        aggregate(&mut root, Metric::Bytes);
        root
    }

    fn home(cache: u64, logs: u64) -> Node {
        scanned(dir(
            "me",
            vec![
                dir(
                    "Library",
                    vec![
                        dir("Caches", vec![file("blob", cache * MIB)]),
                        dir("Logs", vec![file("app.log", logs * MIB)]),
                    ],
                ),
                file("notes.txt", MIB),
            ],
        ))
    }

    #[test]
    fn growth_is_listed_where_it_happened() {
        let store = tempfile::tempdir().unwrap();
        let root = Path::new("/home/me");
        let options = ScanOptions::default();
        record(store.path(), root, &home(100, 20), &options, NOW - 8 * DAY)
            .unwrap();

        let now = home(900, 200);
        let base = baseline(store.path(), root, &options, NOW - 7 * DAY, NOW)
            .unwrap()
            .unwrap();
        let growth = growth(root, &now, &base);

        assert_eq!(growth.since, NOW - 8 * DAY);
        // The cache file grew 800 MiB and the log 180 MiB. Each is listed
        // itself, not `Library` or the directories that only hold it.
        let listed: Vec<_> = growth
            .growers
            .iter()
            .map(|grower| (path_of(root, &now, &grower.crumbs), grower.delta))
            .collect();
        assert_eq!(
            listed,
            [
                (root.join("Library/Caches/blob"), signed(800 * MIB)),
                (root.join("Library/Logs/app.log"), signed(180 * MIB)),
            ]
        );
        assert_eq!(growth.delta(&[0]), Some(signed(980 * MIB)));
        // The note was never big enough to be written down.
        assert_eq!(growth.delta(&[1]), None);
    }

    #[test]
    fn the_first_scan_of_a_day_is_kept() {
        let store = tempfile::tempdir().unwrap();
        let root = Path::new("/home/me");
        let options = ScanOptions::default();
        record(store.path(), root, &home(100, 20), &options, NOW - DAY)
            .unwrap();
        record(store.path(), root, &home(300, 20), &options, NOW - DAY + 60)
            .unwrap();

        assert_eq!(fs::read_dir(store.path()).unwrap().count(), 1);
        let base = baseline(store.path(), root, &options, NOW - DAY, NOW)
            .unwrap()
            .unwrap();
        assert_eq!(
            base.sizes.get(Path::new("/home/me/Library/Caches")),
            Some(&(100 * MIB))
        );
    }

    #[test]
    fn only_comparable_snapshots_are_baselines() {
        let store = tempfile::tempdir().unwrap();
        let options = ScanOptions::default();
        let narrower = Path::new("/home/me/Library");
        record(store.path(), narrower, &home(1, 1), &options, NOW - 9 * DAY)
            .unwrap();
        let without_hidden = ScanOptions {
            include_hidden: false,
            ..ScanOptions::default()
        };
        let root = Path::new("/home/me");
        record(
            store.path(),
            root,
            &home(1, 1),
            &without_hidden,
            NOW - 9 * DAY,
        )
        .unwrap();
        // Just written by the scan on screen: too recent to compare with.
        record(store.path(), root, &home(1, 1), &options, NOW - 60).unwrap();

        let found =
            baseline(store.path(), root, &options, NOW - DAY, NOW).unwrap();
        assert!(found.is_none());
        // A snapshot of a directory above the root covers it.
        let wider = Path::new("/home/me/Library/Caches");
        assert!(
            baseline(store.path(), wider, &options, NOW - DAY, NOW)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn short_history_falls_back_to_the_oldest_snapshot() {
        let store = tempfile::tempdir().unwrap();
        let root = Path::new("/home/me");
        let options = ScanOptions::default();
        for days_ago in [3, 2] {
            record(
                store.path(),
                root,
                &home(1, 1),
                &options,
                NOW - days_ago * DAY,
            )
            .unwrap();
        }
        let base = baseline(store.path(), root, &options, NOW - 30 * DAY, NOW)
            .unwrap()
            .unwrap();
        assert_eq!(base.taken, NOW - 3 * DAY);
    }

    #[test]
    fn old_snapshots_are_deleted() {
        let store = tempfile::tempdir().unwrap();
        let root = Path::new("/home/me");
        let options = ScanOptions::default();
        let old = NOW - (KEEP_DAYS + 1) * DAY;
        record(store.path(), root, &home(1, 1), &options, old).unwrap();
        record(store.path(), root, &home(1, 1), &options, NOW).unwrap();
        assert_eq!(fs::read_dir(store.path()).unwrap().count(), 1);
    }
}
