//! First-open handling of what earlier releases left in the directory.
//!
//! Go's `refstore/migrate.go` imports the Pebble store its earlier releases
//! kept; this crate's earlier releases kept references in a redb database,
//! `<dir>/refs.redb`, and [`migrate_legacy`] imports that, by the same design
//! and with the same guarantees. A Pebble store cannot be imported here and
//! is refused ([`refuse_pebble`]). This file is the only user of redb and
//! goes away with migration support.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use redb::TableDefinition;
use rusqlite::{Connection, Transaction, TransactionBehavior, params};

use super::queries::{CREATE_RECORD, PUT_RECORD};
use super::sqlite::{self, DB_FILE};
use super::{BoxError, Error};

/// The database file of the releases before the shared format.
pub(super) const LEGACY_FILE: &str = "refs.redb";
/// Its one table: name bytes → record bytes.
const LEGACY_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("refs");
/// Keeps the legacy database after the import, as a backup the operator may
/// delete.
pub(super) const LEGACY_DIR: &str = "redb-migrated";
/// Serializes concurrent opens of a store being migrated; the same name Go
/// uses, a BSD `flock(2)` lock.
pub(super) const MIGRATE_LOCK: &str = "migrate.lock";
/// The poison is written here, then renamed over [`LEGACY_FILE`].
const POISON_TMP: &str = "refs.redb.poison.tmp";
/// Prefixes the marker naming Pebble's current manifest: its presence means
/// a Pebble store (Go: `legacyManifest`).
const PEBBLE_MANIFEST: &str = "marker.manifest.";

/// How every poison file starts: redb's magic number, then a marker. One
/// short read per open tells a poison from a legacy database.
///
/// The magic number is what makes the poison work. redb 2.4.0 and later
/// refuse any non-empty file that lacks it, but 2.0.0–2.3.0 take such a file
/// for a new database and overwrite it — an older binary would then see an
/// empty reference store, and a gc run from it would reap every object. With
/// the magic number in place every 2.x release parses the header instead.
pub(super) const POISON_PREFIX: &[u8] =
    b"redb\x1a\x0a\xa9\x0d\x0a\namber-store: references moved to refs.sqlite\n";
/// redb's header: the two transaction slots start at 64 and 192, 128 bytes
/// each, and each slot's first byte is its file format version.
const REDB_HEADER_SIZE: usize = 320;
const REDB_SLOT_VERSION_OFFSETS: [usize; 2] = [64, 192];
/// A file format version no redb release knows: `Database::create` fails
/// with "Expected file format version <= N, found 255" and leaves the file
/// alone — the counterpart of Go's `marker.format-version.999999.999`, which
/// makes Pebble refuse the directory.
const POISON_FORMAT_VERSION: u8 = 0xff;
/// For whoever finds the file.
const POISON_SENTENCE: &str = "amber-store: references moved to refs.sqlite; this file keeps older releases, which would create an empty store here, from opening the directory. The old database is in redb-migrated/.\n";

/// The poison file's bytes: a regular file at `refs.redb` that is not a redb
/// database, so that a binary from before this change fails to open the
/// store instead of silently creating an empty one.
pub(super) fn poison() -> Vec<u8> {
    let mut bytes = POISON_PREFIX.to_vec();
    bytes.resize(REDB_HEADER_SIZE, b'\n');
    for offset in REDB_SLOT_VERSION_OFFSETS {
        bytes[offset] = POISON_FORMAT_VERSION;
    }
    bytes.extend_from_slice(POISON_SENTENCE.as_bytes());
    bytes
}

fn io_err(context: impl Into<String>, e: io::Error) -> Error {
    Error::Migrate {
        context: context.into(),
        source: Box::new(e),
    }
}

fn migrating(e: impl Into<BoxError>) -> Error {
    Error::Migrate {
        context: "refstore: migrating".to_string(),
        source: e.into(),
    }
}

fn reading_legacy(e: impl Into<redb::Error>) -> Error {
    Error::Migrate {
        context: "refstore: reading the redb store".to_string(),
        source: Box::new(e.into()),
    }
}

fn retiring(e: io::Error) -> Error {
    io_err("refstore: retiring the redb store", e)
}

fn exists(path: &Path) -> Result<bool, Error> {
    path.try_exists().map_err(|e| io_err("refstore", e))
}

/// Refuses a directory that holds a Pebble store and no `refs.sqlite`, as
/// `architecture/references.md` demands of an implementation that cannot
/// import Pebble. `refs.sqlite` appears next to Pebble files only by the Go
/// import's atomic rename, so its presence means "imported"; creating an
/// empty one here would hide the references.
pub(super) fn refuse_pebble(dir: &Path) -> Result<(), Error> {
    if exists(&dir.join(DB_FILE))? {
        return Ok(());
    }
    for entry in fs::read_dir(dir).map_err(|e| io_err("refstore", e))? {
        let entry = entry.map_err(|e| io_err("refstore", e))?;
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if !is_dir
            && entry
                .file_name()
                .to_string_lossy()
                .starts_with(PEBBLE_MANIFEST)
        {
            return Err(Error::PebbleStore {
                dir: dir.to_path_buf(),
            });
        }
    }
    Ok(())
}

/// Whether `path` is a legacy database (or debris) rather than absent or the
/// poison. The one cheap check every open pays.
fn is_legacy(path: &Path) -> Result<bool, Error> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(io_err("refstore", e)),
    };
    if !meta.is_file() {
        return Ok(true); // debris; the import reports it
    }
    let mut prefix = Vec::with_capacity(POISON_PREFIX.len());
    File::open(path)
        .and_then(|f| f.take(POISON_PREFIX.len() as u64).read_to_end(&mut prefix))
        .map_err(|e| io_err("refstore", e))?;
    Ok(prefix != POISON_PREFIX)
}

/// Imports a redb reference store found in `dir` and retires it (Go:
/// `migrateLegacy`). Publishing the finished database as `refs.sqlite` is
/// the commit point: before it a crash leaves the redb store untouched;
/// after it the legacy records are only ever merged in, under names the
/// database lacks, so later writes are never overwritten.
pub(super) fn migrate_legacy(dir: &Path, busy_timeout: Duration) -> Result<(), Error> {
    let legacy = dir.join(LEGACY_FILE);
    if !is_legacy(&legacy)? {
        return Ok(());
    }
    let _lock = lock_migration(dir)?;
    // Look again only now: another process may have finished the migration
    // while this one waited for the lock.
    if !is_legacy(&legacy)? {
        return Ok(());
    }
    // Opening the database takes redb's file lock, so a store still held
    // open by an old binary fails the migration instead of being copied
    // mid-flight. The handle stays open until the poison is in place:
    // otherwise an old binary could open the store in the gap and write
    // references that the migration has already left behind.
    let legacy_db = redb::Database::create(&legacy).map_err(|e| match e {
        redb::DatabaseError::DatabaseAlreadyOpen => Error::LegacyInUse {
            path: legacy.clone(),
        },
        e => Error::Migrate {
            context: format!(
                "refstore: opening the redb store {} to migrate it",
                legacy.display()
            ),
            source: Box::new(redb::Error::from(e)),
        },
    })?;
    // Left by a crashed import, whichever way this open goes: the temporary
    // database and its journal. After a crash between the link that publishes
    // the database and the removal of its temporary name, that name is a
    // second one of the live database, which nothing must ever open; taking
    // the name away takes nothing from the database.
    remove_stale(
        dir,
        ["tmp", "tmp-journal"].map(|suffix| format!("{DB_FILE}.{suffix}")),
    )?;
    // A refs.sqlite that is already there proves nothing about the legacy
    // records. It may be this import's own, published just before a crash.
    // It may as well come from an implementation that knows nothing of
    // refs.redb and takes no migration lock: Go creates an empty database
    // in such a directory, and may do so while the import builds its own.
    // So a database that is there is neither trusted, which would hide
    // every legacy reference, nor replaced: the legacy records are merged
    // into it.
    if exists(&dir.join(DB_FILE))? || !import(dir, &legacy_db, busy_timeout)? {
        merge(dir, &legacy_db, busy_timeout)?;
    }
    retire(dir)?;
    drop(legacy_db);
    Ok(())
}

/// Takes the migration lock, blocking (Go: `lockMigration`). Closing the
/// file releases it.
fn lock_migration(dir: &Path) -> Result<File, Error> {
    let lock_err = |e| io_err("refstore: migration lock", e);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o644)
        .open(dir.join(MIGRATE_LOCK))
        .map_err(lock_err)?;
    flock(&file, libc::LOCK_EX).map_err(lock_err)?;
    Ok(file)
}

/// `flock(2)`, retried while interrupted.
fn flock(file: &File, operation: libc::c_int) -> io::Result<()> {
    loop {
        // SAFETY: flock takes a file descriptor and flags; the descriptor is
        // valid for the whole call because `file` is borrowed.
        if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Makes [`hard_link`] fail, as on a filesystem without hard links.
    pub(super) static NO_HARD_LINKS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Runs once, between the import's finished database and its
    /// publication: the moment at which another process creates its own.
    #[allow(clippy::type_complexity)]
    pub(super) static BEFORE_PUBLISH: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

/// `link(2)`.
fn hard_link(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(test)]
    if NO_HARD_LINKS.get() {
        return Err(io::Error::from(io::ErrorKind::Unsupported));
    }
    fs::hard_link(from, to)
}

/// Copies every record into a new database and publishes it as
/// `refs.sqlite` (Go: `importPebble`), unless a database of that name has
/// appeared meanwhile: whether it was published. The copy is built with a
/// rollback journal and full syncs, so once closed it is a single durable
/// file that a name can publish.
fn import(dir: &Path, legacy_db: &redb::Database, busy_timeout: Duration) -> Result<bool, Error> {
    let tmp = dir.join(format!("{DB_FILE}.tmp"));
    // Left by a crashed store whose refs.sqlite was then deleted (the way
    // back to an older release): its WAL and shared-memory index. A moment
    // ago, under the migration lock, there was no refs.sqlite, so they belong
    // to nothing; but SQLite discards a stale WAL only for an empty database,
    // and would replay this one into the finished database published below.
    // Only here: next to a database that is there they are its live ones.
    remove_stale(
        dir,
        ["wal", "shm"].map(|suffix| format!("{DB_FILE}-{suffix}")),
    )?;
    let conn = sqlite::open_db(&tmp, true, false, busy_timeout)?;
    copy_records(&conn, legacy_db, PUT_RECORD)?;
    conn.close().map_err(|(_, e)| migrating(e))?;
    #[cfg(test)]
    if let Some(hook) = BEFORE_PUBLISH.take() {
        hook();
    }
    let published = publish(&tmp, &dir.join(DB_FILE))?; // the commit point
    sync_dir(dir)?;
    Ok(published)
}

/// Removes what is left of the files `names`, if anything.
fn remove_stale(dir: &Path, names: impl IntoIterator<Item = String>) -> Result<(), Error> {
    for name in names {
        match fs::remove_file(dir.join(name)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(io_err("refstore", e)),
            _ => {}
        }
    }
    Ok(())
}

/// Gives the finished database its name unless the name is taken: whether
/// it was published. `link(2)` fails where `rename(2)` would replace, and
/// what it would replace is a database that another process has just created
/// and holds open, whose WAL SQLite would then replay into this one. Either
/// way the temporary name is gone afterwards; after a crash in between, the
/// next open removes it.
fn publish(tmp: &Path, db: &Path) -> Result<bool, Error> {
    let published = match hard_link(tmp, db) {
        Ok(()) => true,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => false,
        // A filesystem without hard links: the check and the rename are two
        // steps there, which is as close as it gets.
        Err(_) if exists(db)? => false,
        Err(_) => {
            fs::rename(tmp, db).map_err(migrating)?;
            return Ok(true);
        }
    };
    fs::remove_file(tmp).map_err(migrating)?;
    Ok(published)
}

/// Adds the legacy records under the names the database that is already
/// there lacks; nothing in it is overwritten or removed. The database is
/// opened the way the store opens it, WAL and schema included, with full
/// syncs: the merge is durable before the poison replaces its source.
fn merge(dir: &Path, legacy_db: &redb::Database, busy_timeout: Duration) -> Result<(), Error> {
    let conn = sqlite::open_db(&dir.join(DB_FILE), true, true, busy_timeout)?;
    copy_records(&conn, legacy_db, CREATE_RECORD)?;
    conn.close().map_err(|(_, e)| migrating(e))?;
    // Whoever created the database may not have synced its directory entry,
    // nor its WAL's; the poison that follows must not outlive them.
    sync_dir(dir)
}

/// Copies every legacy record with `statement`, in one transaction:
/// [`PUT_RECORD`] into the import's own new database, [`CREATE_RECORD`],
/// which leaves a name that exists alone, into one that was already there.
fn copy_records(
    conn: &Connection,
    legacy_db: &redb::Database,
    statement: &str,
) -> Result<(), Error> {
    // Dropped on every early return: the transaction rolls back.
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).map_err(migrating)?;
    {
        let mut put = tx.prepare(statement).map_err(migrating)?;
        let legacy = legacy_db.begin_read().map_err(reading_legacy)?;
        match legacy.open_table(LEGACY_TABLE) {
            Ok(table) => {
                for item in table.range::<&[u8]>(..).map_err(reading_legacy)? {
                    let (name, record) = item.map_err(reading_legacy)?;
                    put.execute(params![name.value(), record.value()])
                        .map_err(migrating)?;
                }
            }
            // An empty file, which redb has just initialized: no table, no
            // references.
            Err(redb::TableError::TableDoesNotExist(_)) => {}
            Err(e) => return Err(reading_legacy(e)),
        }
    }
    tx.commit().map_err(migrating)
}

/// Replaces the legacy database by the poison, keeping it as a backup (Go:
/// `retireLegacy`). The backup is a hard link, or a copy where there are no
/// hard links, and the poison is renamed over the path, which replaces it atomically: there is never a moment at
/// which an old binary finds no file there and creates an empty database. A
/// crash anywhere leaves a legacy file in place, so the next open comes back
/// here and finishes.
fn retire(dir: &Path) -> Result<(), Error> {
    let legacy = dir.join(LEGACY_FILE);
    let aside = dir.join(LEGACY_DIR);
    fs::create_dir_all(&aside).map_err(retiring)?;
    link_backup(&legacy, &aside)?;
    sync_dir(&aside)?;

    let tmp = dir.join(POISON_TMP);
    let written = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(&tmp)
        .and_then(|mut f| {
            f.write_all(&poison())?;
            f.sync_all()
        });
    written.map_err(retiring)?;
    fs::rename(&tmp, &legacy).map_err(retiring)?;
    sync_dir(dir)
}

/// Hard-links the legacy database into the backup directory. A link that is
/// already there — a retirement resumed after a crash — is kept; a different
/// file of that name, the backup of an earlier migration, is left alone and
/// this one gets a numbered name, because the rename that follows removes
/// the legacy database's last other name. Where the link cannot be made —
/// another filesystem behind `redb-migrated`, or one without hard links —
/// the backup is a copy: failing instead would fail every later open after
/// the commit point, with no poison in place and old binaries free to write
/// references that nobody imports.
fn link_backup(legacy: &Path, aside: &Path) -> Result<(), Error> {
    let source = fs::symlink_metadata(legacy).map_err(retiring)?;
    let mut target: PathBuf = aside.join(LEGACY_FILE);
    let mut earlier = Vec::new();
    for n in 1.. {
        match fs::symlink_metadata(&target) {
            Ok(meta) if meta.dev() == source.dev() && meta.ino() == source.ino() => return Ok(()),
            Ok(_) => {
                earlier.push(target);
                target = aside.join(format!("{LEGACY_FILE}.{n}"));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => break,
            Err(e) => return Err(retiring(e)),
        }
    }
    match hard_link(legacy, &target) {
        Ok(()) => Ok(()),
        Err(_) => {
            // A copy cannot be told by its inode. Without this look a
            // retirement that keeps failing after the backup — no room for
            // the poison, say — would add a full copy at every open.
            for earlier in &earlier {
                if same_content(legacy, earlier)? {
                    return Ok(());
                }
            }
            copy_backup(legacy, &target)
        }
    }
}

fn same_content(a: &Path, b: &Path) -> Result<bool, Error> {
    let len = |p: &Path| fs::metadata(p).map(|m| m.len());
    if len(a).map_err(retiring)? != len(b).map_err(retiring)? {
        return Ok(false);
    }
    Ok(fs::read(a).map_err(retiring)? == fs::read(b).map_err(retiring)?)
}

/// The backup as a copy, durable before it gets its name.
fn copy_backup(legacy: &Path, target: &Path) -> Result<(), Error> {
    let mut tmp = target.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::copy(legacy, &tmp).map_err(retiring)?;
    File::open(&tmp)
        .and_then(|f| f.sync_all())
        .map_err(retiring)?;
    fs::rename(&tmp, target).map_err(retiring)
}

fn sync_dir(dir: &Path) -> Result<(), Error> {
    let d = File::open(dir).map_err(|e| io_err("refstore", e))?;
    d.sync_all()
        .map_err(|e| io_err(format!("refstore: syncing {}", dir.display()), e))
}
