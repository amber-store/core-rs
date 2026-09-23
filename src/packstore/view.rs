//! A store's view of a directory that other stores write to (Go:
//! `packstore/view.go`).
//!
//! A store sees a directory that other stores — other processes — write to.
//! Its view is the sealed segments it has mapped plus a read-only index of
//! every active segment it does not own. The view is built at open and goes
//! stale; a lookup that misses refreshes it (`refresh_after_miss`). A reader
//! never takes a lock and never modifies a file.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, MutexGuard, RwLock};
use std::time::{Duration, SystemTime};

use crate::amberpack::REC_HEADER_SIZE;
use crate::key::{self, Key};

use super::footer::SealedSegment;
use super::recover::ActiveLoc;
use super::recover_sidecar::{ScanPos, SegmentScan, advance, read_range, recover_segment};
use super::sidecar::{SIDECAR_SUFFIX, SidecarRec};
use super::{
    ACTIVE_SUFFIX, Error, SEALED_SUFFIX, Shared, Store, be_u32, parse_segment_id, unpoison,
};

/// Marks an active segment that is still being created (`active.rs`) (Go:
/// `tmpSuffix`).
pub(crate) const TMP_SUFFIX: &str = ".tmp";

/// How old the directory's modification time must be, when the directory is
/// listed, before an unchanged time is taken to mean an unchanged directory.
/// On a filesystem with one-second timestamps a segment created in the same
/// second as the listing would otherwise go unnoticed for good. (The trick,
/// and the name, are git's, for its index.) Per store, so that tests can
/// shorten it (Go: `racyWindow`).
pub(crate) const DEFAULT_RACY_WINDOW: Duration = Duration::from_secs(2);

/// Bounds how often one refresh lists the directory, when what it listed
/// keeps vanishing under it (Go: `maxRelist`).
const MAX_RELIST: usize = 3;

/// The name a sealed segment gets: the active segment's without its last
/// extension. On the path itself, not through a string: a store may live
/// under a directory whose name is not UTF-8.
pub(crate) fn sealed_path_of(active: &Path) -> PathBuf {
    active.with_extension("")
}

/// `path` with `suffix` appended to its last component.
pub(crate) fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Go: `segmentName`.
pub(crate) fn segment_name(id: u64, suffix: &str) -> String {
    format!("{id:016x}{suffix}")
}

/// Which file something is: a repair replaces a segment under its name, and
/// an id can come back (Go: the `os.FileInfo` that `os.SameFile` compares).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileIdent {
    dev: u64,
    ino: u64,
}

impl FileIdent {
    pub(crate) fn of(m: &fs::Metadata) -> FileIdent {
        FileIdent {
            dev: m.dev(),
            ino: m.ino(),
        }
    }
}

/// What a lookup in one place found. `Stale` reports that a foreign active
/// segment's index named a record that is not there: the lookup rebuilds the
/// view and tries once more (Go: `errStaleView`).
#[derive(Debug)]
pub(crate) enum Lookup<T> {
    Hit(T),
    Miss,
    Stale,
}

/// What a foreign active segment's view holds beyond the file itself.
#[derive(Debug)]
pub(crate) struct ForeignState {
    pub scan: SegmentScan,
    /// The data file's length at the last look; a different one now means
    /// its owner appended.
    pub seen_size: u64,
}

/// An active segment this store does not own — another writer's, or one
/// nobody holds at the moment — indexed for reading. `state` is written
/// under the store's shared lock, held for writing, and read under it or
/// under the refresh lock (Go: `foreignActive`).
#[derive(Debug)]
pub(crate) struct ForeignActive {
    pub id: u64,
    pub path: PathBuf,
    /// Read-only; follows the file through a seal's rename.
    pub f: File,
    /// Identity: an id can come back as another file.
    pub ident: FileIdent,
    pub state: RwLock<ForeignState>,
}

/// Go: `segFile`.
#[derive(Debug, Clone)]
pub(crate) struct SegFile {
    pub path: PathBuf,
    pub ident: FileIdent,
    pub size: u64,
}

/// Go: `dirListing`.
#[derive(Debug, Default)]
pub(crate) struct DirListing {
    pub sealed: HashMap<u64, SegFile>,
    pub active: HashMap<u64, SegFile>,
    /// File names.
    pub tmp: Vec<OsString>,
    pub sidecars: Vec<OsString>,
    /// Highest segment id of any kind.
    pub max_id: u64,
    /// A name was gone again before it could be looked at: the listing is
    /// out of date.
    pub vanished: bool,
}

/// Go: `listSegments`.
pub(crate) fn list_segments(dir: &Path) -> Result<DirListing, Error> {
    let active_tmp = format!("{ACTIVE_SUFFIX}{TMP_SUFFIX}");
    let active_sidecar = format!("{ACTIVE_SUFFIX}{SIDECAR_SUFFIX}");
    let mut ls = DirListing::default();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name_os = entry.file_name();
        let name = name_os.as_encoded_bytes();
        let (active, suffix) = if name.ends_with(active_tmp.as_bytes()) {
            if let Ok(id) = parse_segment_id(name, &active_tmp) {
                ls.max_id = ls.max_id.max(id);
            }
            ls.tmp.push(name_os);
            continue;
        } else if name.ends_with(active_sidecar.as_bytes()) {
            ls.sidecars.push(name_os);
            continue;
        } else if name.ends_with(ACTIVE_SUFFIX.as_bytes()) {
            (true, ACTIVE_SUFFIX)
        } else if name.ends_with(SEALED_SUFFIX.as_bytes()) {
            (false, SEALED_SUFFIX)
        } else {
            continue; // anything else (.DS_Store, gc.lock, a repair's temporary) is not a segment
        };
        let id = parse_segment_id(name, suffix)?;
        let info = match entry.metadata() {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                ls.vanished = true; // sealed, reaped or renamed since the directory was read
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        let sf = SegFile {
            path: dir.join(&name_os),
            ident: FileIdent::of(&info),
            size: info.len(),
        };
        if active {
            ls.active.insert(id, sf);
        } else {
            ls.sealed.insert(id, sf);
        }
        ls.max_id = ls.max_id.max(id);
    }
    Ok(ls)
}

/// Whether `e` reports a file that is not there.
pub(crate) fn is_not_exist(e: &Error) -> bool {
    match e {
        Error::Io(e) => e.kind() == io::ErrorKind::NotFound,
        Error::Context { source, .. } => is_not_exist(source),
        _ => false,
    }
}

/// What [`open_foreign`] found under an active name.
pub(crate) enum Opened {
    Foreign(ForeignActive),
    /// A file that carries a whole footer — a seal that crashed before its
    /// rename — mapped as the sealed segment it is, under its active name;
    /// its next owner renames it.
    Sealed(Box<SealedSegment>),
    Gone,
}

/// Indexes the active segment at `sf` for reading (Go: `openForeign`).
pub(crate) fn open_foreign(id: u64, sf: &SegFile) -> Result<Opened, Error> {
    let res = match recover_segment(&sf.path) {
        Ok(res) => res,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Opened::Gone),
        Err(e) => return Err(e.into()),
    };
    if res.sealed {
        return match SealedSegment::open(&sf.path, id) {
            Ok(seg) => Ok(Opened::Sealed(Box::new(seg))),
            Err(e) if is_not_exist(&e) => Ok(Opened::Gone),
            Err(e) => Err(e),
        };
    }
    let f = match File::open(&sf.path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Opened::Gone),
        Err(e) => return Err(e.into()),
    };
    let ident = FileIdent::of(&f.metadata()?);
    let seen_size = res.file_size.max(0) as u64;
    Ok(Opened::Foreign(ForeignActive {
        id,
        path: sf.path.clone(),
        f,
        ident,
        state: RwLock::new(ForeignState {
            scan: res.scan(),
            seen_size,
        }),
    }))
}

/// What a poll read: the entries to add, how far it read, and the data
/// file's length.
pub(crate) struct Polled {
    added: Vec<SidecarRec>,
    next: ScanPos,
    size: u64,
}

impl ForeignActive {
    /// Reads what the segment's owner appended since the view last looked.
    /// `None` when the view has to be built again. The view itself is not
    /// touched: the caller commits entries and position together under the
    /// store's lock, or, when the refresh fails on something else, neither —
    /// a position that moved on without its entries would lose them for good
    /// (Go: `poll`).
    fn poll(&self) -> io::Result<Option<Polled>> {
        let st = unpoison(self.state.read());
        let at = st.scan.at;
        let mut tail = Vec::new();
        match File::open(with_suffix(&self.path, SIDECAR_SUFFIX)) {
            // No sidecar, or gone with a seal: the data's tail still reads.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
            Ok(sf) => {
                let len = sf.metadata()?.len() as i64;
                if len < at.sidecar_end {
                    return Ok(None); // started over by a new owner
                }
                tail = read_range(&sf, at.sidecar_end, len - at.sidecar_end)?;
            }
        }
        // After the sidecar: the data covers whatever it speaks of.
        let size = self.f.metadata()?.len();
        Ok(
            advance(&st.scan.index, at, &self.f, size as i64, &tail)?.map(|adv| Polled {
                added: adv.added,
                next: adv.next,
                size,
            }),
        )
    }

    /// Returns the stored payload of the record `loc` names, after checking
    /// that the record there is `k`'s: a view of somebody else's segment is
    /// only as good as the last look at it (Go: `read`).
    pub(crate) fn read(&self, k: Key, loc: ActiveLoc) -> Result<Lookup<Vec<u8>>, Error> {
        Ok(match self.read_record(k, loc)? {
            Lookup::Hit(mut raw) => Lookup::Hit(raw.split_off(REC_HEADER_SIZE)),
            Lookup::Miss => Lookup::Miss,
            Lookup::Stale => Lookup::Stale,
        })
    }

    /// Go: `readRecord`.
    pub(crate) fn read_record(&self, k: Key, loc: ActiveLoc) -> Result<Lookup<Vec<u8>>, Error> {
        let mut raw = vec![0u8; REC_HEADER_SIZE + loc.slen as usize];
        if let Err(e) = self.f.read_exact_at(&mut raw, loc.off) {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                return Ok(Lookup::Stale);
            }
            return Err(e.into());
        }
        if raw[1..1 + key::SIZE] != k.0 || be_u32(&raw, 38) != loc.slen {
            return Ok(Lookup::Stale);
        }
        Ok(Lookup::Hit(raw))
    }
}

/// Adds a segment this store just sealed, adopted or repaired to the view. A
/// refresh may have mapped the same file a moment earlier; that mapping goes
/// (Go: `publishSealedLocked`; the caller holds the shared lock for writing).
pub(crate) fn publish_sealed(sh: &mut Shared, seg: Arc<SealedSegment>) {
    sh.struct_epoch += 1;
    if let Some(slot) = sh.sealed.iter_mut().find(|g| g.id == seg.id) {
        *slot = seg;
        return;
    }
    let id = seg.id;
    sh.sealed.push(seg);
    sh.sealed.sort_by_key(|g| g.id);
    drop_foreign(sh, id);
}

/// Forgets the read-only view of a segment this store now owns or has sealed.
/// A refresh that is reading it at this moment keeps the file open through
/// its own handle (Go: `dropForeignLocked`, where the refresh instead has to
/// cope with a closed file).
pub(crate) fn drop_foreign(sh: &mut Shared, id: u64) {
    sh.foreign.retain(|fa| fa.id != id);
}

/// Deletes the index of a segment that was sealed or wiped: a crash fell
/// between the segment's rename or removal and the index's. A segment's data
/// file exists before its sidecar does and outlives it, so an index without
/// one is never somebody's work in progress (Go: `removeOrphanSidecars`).
pub(crate) fn remove_orphan_sidecars(dir: &Path, ls: &DirListing) {
    let suffix = format!("{ACTIVE_SUFFIX}{SIDECAR_SUFFIX}");
    for name in &ls.sidecars {
        let Ok(id) = parse_segment_id(name.as_encoded_bytes(), &suffix) else {
            continue;
        };
        if !ls.active.contains_key(&id) {
            let _ = fs::remove_file(dir.join(name));
        }
    }
}

impl Store {
    /// Looks at the directory again after a lookup found nothing (or, with
    /// `rebuild`, found a foreign index out of step with its data). Callers
    /// that waited for somebody else's refresh do not repeat it: that listing
    /// is newer than their miss. The result is false when the view was found
    /// current without listing anything, so that the caller need not search
    /// it a second time (Go: `refreshAfterMiss`).
    pub(crate) fn refresh_after_miss(&self, rebuild: bool) -> Result<bool, Error> {
        if !rebuild && self.view_is_current() {
            return Ok(false);
        }
        let seq = self.refresh_seq.load(Ordering::SeqCst);
        let held = unpoison(self.refresh_mu.lock());
        if !rebuild && self.refresh_seq.load(Ordering::SeqCst) != seq {
            return Ok(true);
        }
        self.refresh_locked(&held, rebuild)?;
        Ok(true)
    }

    /// Reports, for the price of a stat or two, that nothing another store
    /// did can have changed what this view holds: the directory has not been
    /// modified since it was listed, so no segment appeared, vanished or was
    /// replaced, and no active segment this store reads has grown. Listing
    /// the directory costs a stat per segment; a lookup that finds nothing is
    /// common enough (any "do you have this?") that it must not pay that
    /// every time (Go: `viewIsCurrent`).
    fn view_is_current(&self) -> bool {
        let (mtime, listed_at, probes) = {
            let sh = unpoison(self.shared.read());
            let probes: Vec<(Arc<ForeignActive>, u64)> = sh
                .foreign
                .iter()
                .map(|fa| (fa.clone(), unpoison(fa.state.read()).seen_size))
                .collect();
            (sh.dir_mtime, sh.listed_at, probes)
        };
        let Some(mtime) = mtime else {
            return false;
        };
        let racy = Duration::from_nanos(self.racy_window_nanos.load(Ordering::Relaxed));
        match listed_at.duration_since(mtime) {
            Ok(age) if age >= racy => {}
            _ => return false, // modified too close to the listing for its time to prove anything
        }
        if fs::metadata(&self.dir).and_then(|m| m.modified()).ok() != Some(mtime) {
            return false;
        }
        probes
            .iter()
            .all(|(fa, size)| fa.f.metadata().is_ok_and(|m| m.len() == *size))
    }

    /// Brings the store's view of the directory up to date: the segments
    /// other stores sealed, created or reaped, and what they appended.
    /// Lookups do it by themselves when they find nothing. A caller that is
    /// about to work from a snapshot of the view, as gc's advisory mark does,
    /// asks for it (Go: `Refresh`).
    pub fn refresh(&self) -> Result<(), Error> {
        let held = unpoison(self.refresh_mu.lock());
        self.refresh_locked(&held, false)
    }

    /// Re-lists the directory and brings the view in line: sealed segments
    /// that appeared are mapped, those that vanished or were replaced are let
    /// go, foreign active segments are read further, indexed or dropped. With
    /// `rebuild` every foreign index is built again. The caller holds the
    /// refresh lock, which `_held` proves.
    ///
    /// What this store itself publishes meanwhile — a seal, an adoption, a
    /// repair — is not in the listing. Such changes bump `struct_epoch`; when
    /// it moved, the refresh only adds and leaves the dropping to the next
    /// one (Go: `refreshLocked`).
    pub(crate) fn refresh_locked(
        &self,
        _held: &MutexGuard<'_, ()>,
        rebuild: bool,
    ) -> Result<(), Error> {
        let (epoch, have, have_foreign, own) = {
            let sh = unpoison(self.shared.read());
            if sh.closed {
                return Err(Error::Closed);
            }
            let have: HashMap<u64, Arc<SealedSegment>> =
                sh.sealed.iter().map(|g| (g.id, g.clone())).collect();
            let have_foreign: HashMap<u64, Arc<ForeignActive>> =
                sh.foreign.iter().map(|fa| (fa.id, fa.clone())).collect();
            (
                sh.struct_epoch,
                have,
                have_foreign,
                sh.active.as_ref().map(|a| a.id),
            )
        };

        struct Delta {
            fa: Arc<ForeignActive>,
            polled: Polled,
        }
        let mut attempt = 1;
        let (dir_mtime, listed_at, opened, opened_foreign, deltas, keep_sealed, keep_foreign) = loop {
            let mut opened: Vec<SealedSegment> = Vec::new();
            let mut opened_foreign: Vec<ForeignActive> = Vec::new();
            let mut deltas: Vec<Delta> = Vec::new();
            let mut keep_sealed: HashSet<u64> = HashSet::new();
            let mut keep_foreign: HashSet<u64> = HashSet::new();
            // The directory's time before its listing: a change that falls
            // between the two then shows as a time this view has not caught
            // up with.
            let dir_mtime = fs::metadata(&self.dir)?.modified().ok();
            let listed_at = SystemTime::now();
            let ls = list_segments(&self.dir)?;
            if let Some(hook) = unpoison(self.hooks.after_list.lock()).as_ref() {
                hook();
            }
            let mut vanished = ls.vanished;
            for (id, sf) in &ls.sealed {
                // A crashed seal that its adopter has renamed is the same
                // file under a new name: it is mapped again, since a mapped
                // segment's path cannot change here (Go updates the path).
                if have
                    .get(id)
                    .is_some_and(|g| g.ident == sf.ident && g.path == sf.path)
                {
                    keep_sealed.insert(*id);
                    continue;
                }
                match SealedSegment::open(&sf.path, *id) {
                    Ok(seg) => opened.push(seg),
                    Err(e) if is_not_exist(&e) => vanished = true, // reaped between the listing and now
                    Err(e) => return Err(e),
                }
            }
            for (id, sf) in &ls.active {
                if own == Some(*id) {
                    continue;
                }
                if have.get(id).is_some_and(|g| g.ident == sf.ident) {
                    keep_sealed.insert(*id); // a crashed seal, mapped under its active name
                    continue;
                }
                if !rebuild
                    && let Some(fa) = have_foreign.get(id)
                    && fa.ident == sf.ident
                    && let Some(polled) = fa.poll()?
                {
                    keep_foreign.insert(*id);
                    deltas.push(Delta {
                        fa: fa.clone(),
                        polled,
                    });
                    continue;
                }
                match open_foreign(*id, sf)? {
                    Opened::Sealed(seg) => opened.push(*seg),
                    Opened::Foreign(fa) => opened_foreign.push(fa),
                    Opened::Gone => vanished = true, // sealed or reaped between the listing and now
                }
            }
            // Something listed was gone when it came to be opened, so the
            // listing is out of date, and what the missing segment held may
            // be in one created since: a compaction's copy, a seal's new
            // name. Look again rather than settle for a view with a hole in
            // it.
            if !vanished || attempt == MAX_RELIST {
                break (
                    dir_mtime,
                    listed_at,
                    opened,
                    opened_foreign,
                    deltas,
                    keep_sealed,
                    keep_foreign,
                );
            }
            attempt += 1;
        };

        let mut sh = unpoison(self.shared.write());
        if sh.closed {
            return Err(Error::Closed);
        }
        let stable = sh.struct_epoch == epoch;
        let mut present: HashSet<u64> = HashSet::new();
        let mut sealed: Vec<Arc<SealedSegment>> = Vec::new();
        for g in std::mem::take(&mut sh.sealed) {
            // Not listed any more: let go of it, unless this store changed
            // the view meanwhile. Whoever still reads it holds its mapping.
            if keep_sealed.contains(&g.id) || !stable {
                present.insert(g.id);
                sealed.push(g);
            }
        }
        for g in opened {
            if present.insert(g.id) {
                sealed.push(Arc::new(g));
            } // else: this store published it first
        }
        sealed.sort_by_key(|g| g.id);

        for d in deltas {
            let mut st = unpoison(d.fa.state.write());
            for r in &d.polled.added {
                st.scan.index.insert(r.k, r.loc());
            }
            st.scan.at = d.polled.next;
            st.seen_size = d.polled.size;
        }
        sh.dir_mtime = dir_mtime;
        sh.listed_at = listed_at;
        let own = sh.active.as_ref().map(|a| a.id);
        let mut foreign: Vec<Arc<ForeignActive>> = Vec::new();
        for fa in std::mem::take(&mut sh.foreign) {
            let keep = keep_foreign.contains(&fa.id) || !stable;
            if own != Some(fa.id) && keep && present.insert(fa.id) {
                foreign.push(fa);
            }
        }
        for fa in opened_foreign {
            if own != Some(fa.id) && present.insert(fa.id) {
                foreign.push(Arc::new(fa));
            }
        }
        sh.sealed = sealed;
        sh.foreign = foreign;
        drop(sh);

        self.refresh_seq.fetch_add(1, Ordering::SeqCst);
        self.refreshes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}
