//! Dev-only mirror of the Go `cmd/amber-store` CLI (no progress UI), for
//! interop testing against the Go binary: same store layout
//! (`<dir>/packstore` + `<dir>/refs`), same spec addressing
//! (`KEY[/PATH]` | `ref:NAME[@PATH]`), same subcommand behavior and
//! `ls -l`-style output.

use std::fs;
use std::io::{self, Seek as _, SeekFrom, Write as _};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Args, Parser, Subcommand};

use amber_store_core::chunkers::ByteOpts;
use amber_store_core::fstree::{self, Entry};
use amber_store_core::gc;
use amber_store_core::ingest;
use amber_store_core::key::Key;
use amber_store_core::packstore;
use amber_store_core::reference::{self, Reference};
use amber_store_core::refstore;
use amber_store_core::{tarexport, tarextract};

type CliError = Box<dyn std::error::Error>;

fn main() {
    if let Err(e) = run() {
        eprintln!("amber-store: {e}");
        std::process::exit(1);
    }
}

/// local content-addressed filesystem tree store
#[derive(Parser)]
#[command(name = "amber-store", disable_help_subcommand = true)]
struct Cli {
    /// store directory (layout: <dir>/packstore, <dir>/refs); defaults to
    /// $AMBER_STORE
    #[arg(long, global = true)]
    store: Option<String>,
    /// pack segment size in bytes; the reaping granularity
    #[arg(
        long = "segment-size",
        global = true,
        default_value_t = packstore::DEFAULT_SEGMENT_SIZE
    )]
    segment_size: u64,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// build the content-addressed tree for PATH (a directory or a single
    /// file), store it, and print the root key
    Ingest(IngestArgs),
    /// list the entries of the directory object KEY, or of the subdirectory
    /// PATH within it; accepts a reference as ref:NAME[@PATH]
    Ls(LsArgs),
    /// write the tree rooted at KEY, or at the subdirectory PATH within it,
    /// as a PAX tar to stdout; accepts a reference as ref:NAME[@PATH]
    Export(ExportArgs),
    /// restore the filesystem tree rooted at KEY, or at the subdirectory
    /// PATH within it, into DIR; accepts a reference as ref:NAME[@PATH]
    Restore(RestoreArgs),
    /// manage references: named pointers to root keys
    #[command(subcommand)]
    Ref(RefCmd),
    /// garbage collection: score packs, reap the mostly-dead ones
    #[command(subcommand)]
    Gc(GcCmd),
}

#[derive(Args)]
struct IngestArgs {
    /// ultracdc minimum chunk size in bytes
    #[arg(long, default_value_t = amber_store_core::chunkers::DEFAULT_MIN_SIZE as i64)]
    min: i64,
    /// ultracdc average (normal) chunk size in bytes
    #[arg(long, default_value_t = amber_store_core::chunkers::DEFAULT_NORMAL_SIZE as i64)]
    avg: i64,
    /// ultracdc maximum chunk size in bytes
    #[arg(long, default_value_t = amber_store_core::chunkers::DEFAULT_MAX_SIZE as i64)]
    max: i64,
    /// item chunker average run = 2^bits
    #[arg(long = "item-bits", default_value_t = ingest::DEFAULT_ITEM_BITS)]
    item_bits: u32,
    /// xattrs larger than this many bytes spill to an XattrSet
    #[arg(long = "xattr-inline-max", default_value_t = ingest::DEFAULT_XATTR_INLINE_MAX)]
    xattr_inline_max: usize,
    /// record the resolved root under reference NAME
    #[arg(long = "ref")]
    reference: Option<String>,
    /// concurrent workers building the tree (default: number of CPUs)
    #[arg(long, short = 'j', default_value_t = default_jobs())]
    jobs: usize,
    /// do not honor .amberignore files
    #[arg(long = "no-ignore")]
    no_ignore: bool,
    /// accepted for command-line parity with the Go CLI; this build has no
    /// progress UI
    #[arg(long = "no-progress", hide = true)]
    no_progress: bool,
    path: PathBuf,
}

#[derive(Args)]
struct LsArgs {
    /// append each entry's content key (usable as KEY for ls/export/restore)
    #[arg(long)]
    keys: bool,
    /// KEY[/PATH] | ref:NAME[@PATH]
    spec: String,
}

#[derive(Args)]
struct ExportArgs {
    /// write the tar to FILE instead of stdout
    #[arg(long, short = 'o')]
    output: Option<PathBuf>,
    /// KEY[/PATH] | ref:NAME[@PATH]
    spec: String,
}

#[derive(Args)]
struct RestoreArgs {
    /// KEY[/PATH] | ref:NAME[@PATH]
    spec: String,
    /// destination directory
    dir: PathBuf,
}

#[derive(Subcommand)]
enum RefCmd {
    /// list every reference: name, key, creation time, creator
    List,
    /// print the key a reference points at
    Get { name: String },
    /// create or overwrite reference NAME pointing at KEY
    Set { name: String, key: String },
    /// delete reference NAME
    Rm { name: String },
}

#[derive(Subcommand)]
enum GcCmd {
    /// packs: id, sealed, bytes, garbage, eligible; totals; closures;
    /// union; last cycle
    Status,
    /// score now, reap packs above the garbage line
    Run(GcRunArgs),
    /// references whose closure holds KEY's tail
    Why {
        /// the object key to explain (exactly one)
        #[arg(value_name = "KEY")]
        key: Vec<String>,
    },
}

#[derive(Args)]
struct GcRunArgs {
    /// force the selection line (fraction; default: 0.5, or 0.1 under
    /// min-free pressure)
    #[arg(long, default_value_t = -1.0, allow_negative_numbers = true)]
    garbage: f64,
    /// minimum age of a sealed pack before it can be reaped
    #[arg(
        long,
        default_value = "1h",
        value_parser = parse_go_duration,
        allow_hyphen_values = true
    )]
    grace: i64,
    /// copier bandwidth cap in bytes/s (0 = unlimited)
    #[arg(long, default_value_t = 0, allow_negative_numbers = true)]
    rate: i64,
    /// free-space floor in bytes (0 = 5% of the filesystem)
    #[arg(long, default_value_t = 0)]
    min_free: u64,
}

fn default_jobs() -> usize {
    thread::available_parallelism().map_or(1, |n| n.get())
}

fn run() -> Result<(), CliError> {
    let cli = Cli::parse();
    match &cli.cmd {
        Cmd::Ingest(a) => run_ingest(&cli, a),
        Cmd::Ls(a) => run_ls(&cli, a),
        Cmd::Export(a) => run_export(&cli, a),
        Cmd::Restore(a) => run_restore(&cli, a),
        Cmd::Ref(r) => run_ref(&cli, r),
        Cmd::Gc(g) => run_gc(&cli, g),
    }
}

// ---------------------------------------------------------------------------
// Store plumbing (Go: store.go).
// ---------------------------------------------------------------------------

struct Stores {
    // Arcs so a collector can share the open stores (Go passes the same
    // pointers); each underlying store stays single-owner per process.
    objects: Arc<packstore::Store>,
    refs: Arc<refstore::Store>,
}

/// Resolves the store directory from --store or $AMBER_STORE (Go:
/// urfave/cli's EnvVars fallback on the --store flag; `openCollector` reads
/// the same resolved value).
fn store_dir(cli: &Cli) -> Result<PathBuf, CliError> {
    let dir = match &cli.store {
        Some(d) => d.clone(),
        None => std::env::var("AMBER_STORE").unwrap_or_default(),
    };
    if dir.is_empty() {
        return Err("no store directory: set --store or $AMBER_STORE".into());
    }
    Ok(PathBuf::from(dir))
}

/// Opens (creating as needed) the store directory named by --store or
/// $AMBER_STORE: `<dir>/packstore` holds the objects, `<dir>/refs` the
/// references DB. Stores are single-owner: never open one directory from two
/// live processes.
fn open_store(cli: &Cli) -> Result<Stores, CliError> {
    let dir = store_dir(cli)?;
    let objects = packstore::Store::open_with(
        dir.join("packstore"),
        packstore::Options::new()
            .sync(true)
            .segment_size(cli.segment_size),
    )?;
    let refs = match refstore::Store::open(dir.join("refs"), true) {
        Ok(r) => r,
        Err(e) => {
            let _ = objects.close();
            return Err(e.into());
        }
    };
    Ok(Stores {
        objects: Arc::new(objects),
        refs: Arc::new(refs),
    })
}

/// Closes both halves (the refs DB closes on drop). A collector opened next
/// to these stores must be closed — and dropped, releasing its store
/// handles — first.
fn close_store(st: Stores) -> Result<(), CliError> {
    drop(st.refs);
    st.objects.close()?;
    Ok(())
}

/// Opens the collector next to an already-open store pair;
/// `<dir>/closures` holds the closure files. Close it before
/// [`close_store`] (Go: `openCollector`).
fn open_collector(cli: &Cli, st: &Stores, opts: gc::Options) -> Result<gc::Collector, CliError> {
    Ok(gc::Collector::open(
        store_dir(cli)?.join("closures"),
        Arc::clone(&st.objects),
        Arc::clone(&st.refs),
        opts,
    )?)
}

/// Joins the failures' messages with newlines, mirroring the Go CLI's
/// `errors.Join(err, coll.Close(), closeStore(...))` teardown shape: every
/// error is reported, none masks another.
fn join_errs<const N: usize>(results: [Result<(), CliError>; N]) -> Result<(), CliError> {
    let mut msgs = Vec::new();
    for r in results {
        if let Err(e) = r {
            msgs.push(e.to_string());
        }
    }
    if msgs.is_empty() {
        Ok(())
    } else {
        Err(msgs.join("\n").into())
    }
}

// ---------------------------------------------------------------------------
// Spec resolution (Go: spec.go).
// ---------------------------------------------------------------------------

/// Parses a content spec: either KEY[/PATH] (lowercase-hex key,
/// slash-separated subpath) or ref:NAME[@PATH] (reference name,
/// '@'-separated subpath — '@' is banned in names, so the first '@' is
/// unambiguous). Reference names resolve through the store's references DB.
fn resolve_spec(refs: &refstore::Store, s: &str) -> Result<(Key, String), CliError> {
    let Some(rest) = s.strip_prefix("ref:") else {
        return parse_key_path(s);
    };
    let (name, path) = match rest.split_once('@') {
        Some((n, p)) => (n, p),
        None => (rest, ""),
    };
    reference::validate_name(name).map_err(|e| format!("invalid reference spec {s:?}: {e}"))?;
    let raw = refs.get(name)?;
    let rec = Reference::decode(&raw).map_err(|e| format!("reference {name:?}: {e}"))?;
    let k = Key::parse(&rec.key).map_err(|e| format!("reference {name:?}: stored key: {e}"))?;
    Ok((k, path.to_string()))
}

/// Splits a KEY[/PATH] argument at the first slash and decodes the key part.
/// The returned path is empty when no slash follows the key.
fn parse_key_path(s: &str) -> Result<(Key, String), CliError> {
    let (key_part, path) = match s.split_once('/') {
        Some((k, p)) => (k, p),
        None => (s, ""),
    };
    Ok((parse_hex_key(key_part)?, path.to_string()))
}

/// Decodes a lowercase-hex key argument into a validated key.
fn parse_hex_key(s: &str) -> Result<Key, CliError> {
    let raw = hex::decode(s).map_err(|e| format!("invalid key {s:?}: {e}"))?;
    let k = Key::parse(&raw).map_err(|e| format!("invalid key {s:?}: {e}"))?;
    Ok(k)
}

/// Resolves a slash-separated subpath from `root` and returns the target
/// entry's content key. Every traversed segment must be an entry carrying a
/// content key (a regular file or a directory).
fn descend(objects: &packstore::Store, root: Key, path: &str) -> Result<Key, CliError> {
    let mut k = root;
    for seg in path.split('/') {
        if seg.is_empty() {
            continue;
        }
        let e = fstree::lookup_entry(k, seg.as_bytes(), |kk| objects.get(kk))
            .map_err(|e| format!("resolving {path:?}: {e}"))?;
        k = Key::parse(&e.content_key)
            .map_err(|_| format!("resolving {path:?}: {seg:?} is not a file or directory"))?;
    }
    Ok(k)
}

// ---------------------------------------------------------------------------
// ingest (Go: ingest.go + the chunk flags in main.go).
// ---------------------------------------------------------------------------

/// Maps the CLI chunking flags onto the library options. min/avg/max must
/// all be set together or all left zero (the library defaults).
fn chunk_opts(a: &IngestArgs) -> Result<ingest::ChunkOpts, CliError> {
    let mut opts = ingest::ChunkOpts {
        byte: None,
        item_bits: a.item_bits,
        xattr_inline_max: a.xattr_inline_max,
    };
    if a.min == 0 && a.avg == 0 && a.max == 0 {
        return Ok(opts);
    }
    if a.min <= 0 || a.avg <= 0 || a.max <= 0 {
        return Err("--min, --avg and --max must all be set together".into());
    }
    opts.byte = Some(ByteOpts {
        min_size: a.min as usize,
        normal_size: a.avg as usize,
        max_size: a.max as usize,
        key: Vec::new(),
    });
    Ok(opts)
}

fn run_ingest(cli: &Cli, a: &IngestArgs) -> Result<(), CliError> {
    if let Some(name) = &a.reference {
        reference::validate_name(name)?;
    }
    let chunk = chunk_opts(a)?;
    let st = open_store(cli)?;
    let opts = ingest::Opts {
        jobs: a.jobs,
        chunk,
        no_ignore: a.no_ignore,
        progress: None,
        exclude: Vec::new(),
    };
    let (_stats, res) = ingest::dir(&st.objects, &a.path, opts);
    let root = match res {
        Ok(k) => k,
        Err(e) => {
            let _ = close_store(st);
            return Err(e.into());
        }
    };
    if let Some(name) = &a.reference {
        let rec = Reference {
            name: name.clone(),
            key: root.as_bytes().to_vec(),
            created_at: now_unix_nanos(),
            ..Default::default()
        };
        let put = rec.encode().map_err(CliError::from).and_then(|raw| {
            // The reference is published through the collector, so an
            // incomplete tree can fail the write — shouldn't happen right
            // after ingest (Go: runIngest's openCollector + putRef +
            // coll.Close join).
            let coll = open_collector(cli, &st, gc::Options::default())?;
            let res = put_ref(&coll, &st.refs, name, root, &raw);
            let closed = coll.close().map_err(CliError::from);
            join_errs([res, closed])
        });
        if let Err(e) = put {
            let _ = close_store(st);
            return Err(format!(
                "tree stored (root {root}) but creating reference {name:?} failed: {e}\n\
                 retry with: amber-store ref set {name:?} {root}"
            )
            .into());
        }
    }
    close_store(st)?;
    println!("{root}");
    Ok(())
}

fn now_unix_nanos() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i64,
        Err(e) => -(e.duration().as_nanos() as i64),
    }
}

// ---------------------------------------------------------------------------
// ls (Go: ls.go).
// ---------------------------------------------------------------------------

/// Bounds one list_entries page; run_ls loops until the listing is drained,
/// so it only caps memory per fetch, not the output.
const LS_PAGE_SIZE: usize = 4096;

fn run_ls(cli: &Cli, a: &LsArgs) -> Result<(), CliError> {
    let st = open_store(cli)?;
    let res = ls_inner(&st, a);
    let close = close_store(st);
    res?;
    close
}

fn ls_inner(st: &Stores, a: &LsArgs) -> Result<(), CliError> {
    let (k, path) = resolve_spec(&st.refs, &a.spec)?;
    let dir = descend(&st.objects, k, &path)?;
    let mut entries: Vec<Entry> = Vec::new();
    let mut after: Vec<u8> = Vec::new();
    loop {
        let (page, more) =
            fstree::list_entries(dir, &after, LS_PAGE_SIZE, |kk| st.objects.get(kk))?;
        entries.extend(page);
        if !more {
            break;
        }
        after = entries.last().map(|e| e.name.clone()).unwrap_or_default();
    }
    render_ls(&mut io::stdout().lock(), &entries, now_unix_nanos(), a.keys)?;
    Ok(())
}

/// Writes one `ls -l` style line per entry: mode, uid, gid, size, mtime and
/// name (with the symlink target after "->"). Numeric columns are
/// right-aligned to the widest value. When `show_keys` is set, each entry's
/// content key (if any) is appended after the name. Names are raw bytes,
/// exactly as Go prints them.
fn render_ls(
    w: &mut dyn io::Write,
    entries: &[Entry],
    now_ns: i64,
    show_keys: bool,
) -> io::Result<()> {
    let (mut uid_w, mut gid_w, mut size_w) = (0, 0, 0);
    for e in entries {
        uid_w = uid_w.max(e.uid.to_string().len());
        gid_w = gid_w.max(e.gid.to_string().len());
        size_w = size_w.max(size_string(e).len());
    }
    for e in entries {
        let mut line = Vec::new();
        line.extend_from_slice(mode_string(e.mode).as_bytes());
        line.extend_from_slice(
            format!(
                " {:>uw$} {:>gw$} {:>sw$} {} ",
                e.uid,
                e.gid,
                size_string(e),
                format_mtime(e.mtime, now_ns),
                uw = uid_w,
                gw = gid_w,
                sw = size_w,
            )
            .as_bytes(),
        );
        line.extend_from_slice(&e.name);
        if !e.link_target.is_empty() {
            line.extend_from_slice(b" -> ");
            line.extend_from_slice(&e.link_target);
        }
        // An absent (empty) content key simply fails to parse.
        let shown_key = if show_keys {
            Key::parse(&e.content_key).ok()
        } else {
            None
        };
        if let Some(ck) = shown_key {
            line.push(b' ');
            line.extend_from_slice(ck.to_string().as_bytes());
        }
        line.push(b'\n');
        w.write_all(&line)?;
    }
    Ok(())
}

/// Renders the size column: "major,minor" for device entries, the content
/// length carried by the entry's key otherwise (a directory key's length
/// counts its entries). Symlinks show their target length.
fn size_string(e: &Entry) -> String {
    if e.rdev.len() == 2 {
        return format!("{},{}", e.rdev[0], e.rdev[1]);
    }
    // An absent (empty) content key simply fails to parse, so no emptiness
    // pre-check is needed.
    if let Ok(ck) = Key::parse(&e.content_key) {
        return ck.length().to_string();
    }
    e.link_target.len().to_string()
}

const S_IFMT: u64 = libc::S_IFMT as u64;
const S_IFDIR: u64 = libc::S_IFDIR as u64;
const S_IFLNK: u64 = libc::S_IFLNK as u64;
const S_IFCHR: u64 = libc::S_IFCHR as u64;
const S_IFBLK: u64 = libc::S_IFBLK as u64;
const S_IFIFO: u64 = libc::S_IFIFO as u64;
const S_IFSOCK: u64 = libc::S_IFSOCK as u64;
const S_IFREG: u64 = libc::S_IFREG as u64;
const S_ISUID: u64 = libc::S_ISUID as u64;
const S_ISGID: u64 = libc::S_ISGID as u64;
const S_ISVTX: u64 = libc::S_ISVTX as u64;

/// Renders a raw POSIX st_mode the way `ls -l` does: a type character
/// followed by nine permission characters, with setuid/setgid/sticky folded
/// into the corresponding execute slots.
fn mode_string(mode: u64) -> String {
    let mut b = [0u8; 10];
    b[0] = match mode & S_IFMT {
        S_IFDIR => b'd',
        S_IFLNK => b'l',
        S_IFCHR => b'c',
        S_IFBLK => b'b',
        S_IFIFO => b'p',
        S_IFSOCK => b's',
        S_IFREG => b'-',
        _ => b'?',
    };
    const RWX: &[u8; 9] = b"rwxrwxrwx";
    for (i, &c) in RWX.iter().enumerate() {
        b[1 + i] = if mode & (1 << (8 - i)) != 0 { c } else { b'-' };
    }
    let mut set_bit = |pos: usize, bit: u64, with_x: u8, without_x: u8| {
        if mode & bit == 0 {
            return;
        }
        b[pos] = if b[pos] == b'x' { with_x } else { without_x };
    };
    set_bit(3, S_ISUID, b's', b'S');
    set_bit(6, S_ISGID, b's', b'S');
    set_bit(9, S_ISVTX, b't', b'T');
    String::from_utf8_lossy(&b).into_owned()
}

/// The local calendar fields of a Unix timestamp, via libc so the timezone
/// database is consulted exactly like Go's time package does.
fn local_tm(secs: i64) -> libc::tm {
    let t = secs as libc::time_t;
    // SAFETY: localtime_r fills the out-param and touches nothing else.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    }
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Renders an mtime the way `ls -l` (and the Go CLI) does: "Jan _2 15:04"
/// for times within the last six months, "Jan _2  2006" for older or future
/// times. The six-month cutoff mirrors Go's `now.AddDate(0, -6, 0)` via
/// mktime's field normalization.
fn format_mtime(t_ns: i64, now_ns: i64) -> String {
    let cutoff_ns = {
        let mut tm = local_tm(now_ns.div_euclid(1_000_000_000));
        tm.tm_mon -= 6;
        tm.tm_isdst = -1;
        // SAFETY: mktime normalizes the tm we own; no aliasing.
        let secs = unsafe { libc::mktime(&mut tm) } as i64;
        secs * 1_000_000_000 + now_ns.rem_euclid(1_000_000_000)
    };
    let tm = local_tm(t_ns.div_euclid(1_000_000_000));
    let mon = MONTHS
        .get(tm.tm_mon.clamp(0, 11) as usize)
        .copied()
        .unwrap_or("???");
    if t_ns > now_ns || t_ns < cutoff_ns {
        format!("{mon} {:>2}  {}", tm.tm_mday, i64::from(tm.tm_year) + 1900)
    } else {
        format!("{mon} {:>2} {:02}:{:02}", tm.tm_mday, tm.tm_hour, tm.tm_min)
    }
}

// ---------------------------------------------------------------------------
// export / restore (Go: export.go, restore.go).
// ---------------------------------------------------------------------------

fn run_export(cli: &Cli, a: &ExportArgs) -> Result<(), CliError> {
    let st = open_store(cli)?;
    let res = export_inner(&st, a);
    let close = close_store(st);
    res?;
    close
}

fn export_inner(st: &Stores, a: &ExportArgs) -> Result<(), CliError> {
    let (k, path) = resolve_spec(&st.refs, &a.spec)?;
    let dir = descend(&st.objects, k, &path)?;
    match &a.output {
        Some(out) => {
            let mut f = fs::File::create(out)?;
            tarexport::write(&mut f, dir, |kk| st.objects.get(kk))?;
            f.sync_all()?; // surface a flush error on the happy path
            Ok(())
        }
        None => {
            let mut w = io::stdout().lock();
            tarexport::write(&mut w, dir, |kk| st.objects.get(kk))?;
            Ok(())
        }
    }
}

fn run_restore(cli: &Cli, a: &RestoreArgs) -> Result<(), CliError> {
    let st = open_store(cli)?;
    let res = restore_inner(&st, a);
    let close = close_store(st);
    res?;
    close
}

fn restore_inner(st: &Stores, a: &RestoreArgs) -> Result<(), CliError> {
    let (k, path) = resolve_spec(&st.refs, &a.spec)?;
    let dir = descend(&st.objects, k, &path)?;

    // The Go CLI pipes tarexport straight into tarextract. Here the export
    // is spooled through an unlinked temp file instead of a pipe (the
    // toolchain floor predates std::io::pipe), which keeps memory flat and
    // surfaces an export error before extraction starts, like the pipe's
    // CloseWithError does.
    let mut spool = tempfile::tempfile()?;
    tarexport::write(&mut spool, dir, |kk| st.objects.get(kk))?;
    spool.seek(SeekFrom::Start(0))?;
    tarextract::extract(&mut spool, &a.dir)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// ref (Go: ref.go).
// ---------------------------------------------------------------------------

fn run_ref(cli: &Cli, cmd: &RefCmd) -> Result<(), CliError> {
    match cmd {
        RefCmd::List => {
            let st = open_store(cli)?;
            let res = ref_list(&st);
            let close = close_store(st);
            res?;
            close
        }
        RefCmd::Get { name } => {
            let st = open_store(cli)?;
            let res = resolve_spec(&st.refs, &format!("ref:{name}"));
            let close = close_store(st);
            let (k, _) = res?;
            println!("{k}");
            close
        }
        RefCmd::Set { name, key } => {
            let k = parse_hex_key(key)?;
            let rec = Reference {
                name: name.clone(),
                key: k.as_bytes().to_vec(),
                created_at: now_unix_nanos(),
                ..Default::default()
            };
            let raw = rec.encode()?;
            let st = open_store(cli)?;
            let coll = match open_collector(cli, &st, gc::Options::default()) {
                Ok(c) => c,
                Err(e) => {
                    let _ = close_store(st);
                    return Err(e);
                }
            };
            let res = put_ref(&coll, &st.refs, name, k, &raw);
            let closed = coll.close().map_err(CliError::from);
            drop(coll); // release the collector's store handles first
            join_errs([res, closed, close_store(st)])
        }
        RefCmd::Rm { name } => {
            let st = open_store(cli)?;
            let coll = match open_collector(cli, &st, gc::Options::default()) {
                Ok(c) => c,
                Err(e) => {
                    let _ = close_store(st);
                    return Err(e);
                }
            };
            let res = rm_ref(&coll, &st.refs, name);
            let closed = coll.close().map_err(CliError::from);
            drop(coll);
            join_errs([res, closed, close_store(st)])
        }
    }
}

fn ref_list(st: &Stores) -> Result<(), CliError> {
    let records = st.refs.all()?;
    let mut out = io::stdout().lock();
    for r in records {
        let rec = Reference::decode(&r.data).map_err(|e| format!("reference {:?}: {e}", r.name))?;
        let k =
            Key::parse(&rec.key).map_err(|e| format!("reference {:?}: stored key: {e}", r.name))?;
        let mut line = format!("{} {} {}", rec.name, k, rfc3339_utc(rec.created_at));
        if !rec.user.is_empty() {
            line.push(' ');
            line.push_str(&rec.user);
        }
        writeln!(out, "{line}")?;
    }
    Ok(())
}

/// Writes a reference under the collector's removal lock: the closure is
/// reused or walked — a missing object fails the write, naming it — the
/// record is stored, and an overwritten root is released. This is the
/// optimistic reference PUT: on a 404 the caller re-sends the missing
/// objects and retries (Go: `putRef`).
///
/// Calls for one name must be serialized by the caller (the one-shot CLI
/// is); the read-old -> prepare -> put -> release sequence is not atomic
/// against a concurrent writer of the same name.
fn put_ref(
    coll: &gc::Collector,
    refs: &refstore::Store,
    name: &str,
    root: Key,
    raw: &[u8],
) -> Result<(), CliError> {
    let mut old: Option<Key> = None;
    match refs.get(name) {
        Ok(prev) => {
            let prev_ref = Reference::decode(&prev)
                .map_err(|e| format!("existing reference {name:?}: {e}"))?;
            let k = Key::parse(&prev_ref.key)
                .map_err(|e| format!("existing reference {name:?}: {e}"))?;
            old = Some(k);
        }
        Err(e) if e.is_not_found() => {}
        Err(e) => return Err(e.into()),
    }
    let prepared = coll.prepare_ref(root)?;
    if let Err(e) = refs.put(name, raw) {
        prepared.abort();
        return Err(e.into());
    }
    prepared.commit();
    if let Some(old) = old {
        coll.release_ref(old)?;
    }
    Ok(())
}

/// Deletes a reference and releases its root: the tails leave the union;
/// the closure file goes if no other name shares the root. No walk (Go:
/// `rmRef`).
fn rm_ref(coll: &gc::Collector, refs: &refstore::Store, name: &str) -> Result<(), CliError> {
    let prev = refs.get(name)?;
    let rec = Reference::decode(&prev).map_err(|e| format!("reference {name:?}: {e}"))?;
    let root = Key::parse(&rec.key).map_err(|e| format!("reference {name:?}: {e}"))?;
    refs.delete(name)?;
    coll.release_ref(root)?;
    Ok(())
}

/// Formats a ns-precision Unix timestamp the way Go's
/// `time.Unix(0, ns).UTC().Format(time.RFC3339)` does (seconds precision,
/// trailing "Z").
fn rfc3339_utc(ns: i64) -> String {
    let secs = ns.div_euclid(1_000_000_000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Proleptic-Gregorian date from days since 1970-01-01 (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

// ---------------------------------------------------------------------------
// gc (Go: gc.go; humanBytes from progress.go).
// ---------------------------------------------------------------------------

fn run_gc(cli: &Cli, cmd: &GcCmd) -> Result<(), CliError> {
    match cmd {
        GcCmd::Status => run_gc_status(cli),
        GcCmd::Run(a) => run_gc_run(cli, a),
        GcCmd::Why { key } => run_gc_why(cli, key),
    }
}

/// Prints the pack table, the totals, and the last cycle (Go:
/// `runGCStatus`; its defers drop the close errors, and so does this).
fn run_gc_status(cli: &Cli) -> Result<(), CliError> {
    let st = open_store(cli)?;
    let coll = match open_collector(cli, &st, gc::Options::default()) {
        Ok(c) => c,
        Err(e) => {
            let _ = close_store(st);
            return Err(e);
        }
    };
    let res = gc_status_print(&coll);
    let _ = coll.close();
    drop(coll);
    let _ = close_store(st);
    res
}

/// The `gc status` report. Go clamps `GarbageBytes`/`FreedBytes` with
/// `max(x, 0)` before humanizing; the Rust counters are unsigned, so the
/// clamp has no counterpart.
fn gc_status_print(coll: &gc::Collector) -> Result<(), CliError> {
    let status = coll.status()?;
    let mut w = io::stdout().lock();
    writeln!(
        w,
        "{:<16}  {:<20}  {:>10}  {:>7}  ELIGIBLE",
        "PACK", "SEALED", "BYTES", "GARBAGE"
    )?;
    for p in &status.packs {
        writeln!(
            w,
            "{:016x}  {:<20}  {:>10}  {:>6.1}%  {}",
            p.id,
            rfc3339_local(p.sealed),
            human_bytes(p.body),
            100.0 * p.garbage,
            p.eligible
        )?;
    }
    writeln!(
        w,
        "live {}, garbage {}; {} refs, {} live objects marked",
        human_bytes(status.live_bytes),
        human_bytes(status.garbage_bytes),
        status.refs,
        status.marked
    )?;
    if let Some(last) = &status.last {
        writeln!(
            w,
            "last cycle: {}, {} packs scored, {} reaped, {} copied, {} freed",
            rfc3339_local(last.start),
            last.scored,
            last.reaped.len(),
            human_bytes(last.copied_bytes),
            human_bytes(last.freed_bytes)
        )?;
    }
    if let Some(e) = &status.last_error {
        writeln!(w, "last cycle error: {e}")?;
    }
    Ok(())
}

/// Runs one cycle and prints its stats line (Go: `runGCRun`).
fn run_gc_run(cli: &Cli, a: &GcRunArgs) -> Result<(), CliError> {
    let st = open_store(cli)?;
    let opts = gc::Options {
        // A negative --grace clamps to zero; both select the default, as
        // Go's withDefaults does for Grace <= 0.
        grace: Duration::from_nanos(a.grace.max(0) as u64),
        min_free: a.min_free,
        rate: a.rate,
        ..Default::default()
    };
    let coll = match open_collector(cli, &st, opts) {
        Ok(c) => c,
        Err(e) => {
            let _ = close_store(st);
            return Err(e);
        }
    };
    let res = coll.run(a.garbage).map_err(CliError::from).map(|stats| {
        println!(
            "{} packs scored, {} reaped, {} records ({}) copied, {} freed in {} (mark {}, sweep {}; {} objects marked)",
            stats.scored,
            stats.reaped.len(),
            stats.copied_records,
            human_bytes(stats.copied_bytes),
            human_bytes(stats.freed_bytes),
            format_go_duration(round_ms(go_ns(stats.duration))),
            format_go_duration(round_ms(go_ns(stats.mark_duration))),
            format_go_duration(round_ms(go_ns(stats.sweep_duration))),
            stats.marked
        );
    });
    let _ = coll.close();
    drop(coll);
    let _ = close_store(st);
    res
}

/// Prints the references that keep KEY alive, or "unreferenced" (Go:
/// `runGCWhy`; an unreferenced key still exits 0).
fn run_gc_why(cli: &Cli, keys: &[String]) -> Result<(), CliError> {
    if keys.len() != 1 {
        return Err(format!(
            "gc why requires exactly one KEY argument, got {}",
            keys.len()
        )
        .into());
    }
    let k = parse_hex_key(&keys[0])?;
    let st = open_store(cli)?;
    let coll = match open_collector(cli, &st, gc::Options::default()) {
        Ok(c) => c,
        Err(e) => {
            let _ = close_store(st);
            return Err(e);
        }
    };
    let res = gc_why_print(&coll, k);
    let _ = coll.close();
    drop(coll);
    let _ = close_store(st);
    res
}

fn gc_why_print(coll: &gc::Collector, k: Key) -> Result<(), CliError> {
    let names = coll.why(k)?;
    let mut w = io::stdout().lock();
    if names.is_empty() {
        writeln!(w, "unreferenced")?;
        return Ok(());
    }
    for n in &names {
        writeln!(w, "{n}")?;
    }
    Ok(())
}

/// Formats n with binary (KiB/MiB/…) units (Go: `humanBytes` in
/// progress.go — this build has no progress UI, but the gc report reuses
/// the same helper).
fn human_bytes(n: u64) -> String {
    const UNIT: u64 = 1024;
    if n < UNIT {
        return format!("{n} B");
    }
    const UNITS: [char; 6] = ['K', 'M', 'G', 'T', 'P', 'E'];
    let (mut div, mut exp) = (UNIT, 0usize);
    let mut m = n / UNIT;
    while m >= UNIT {
        div *= UNIT;
        exp += 1;
        m /= UNIT;
    }
    let mut val = n as f64 / div as f64;
    // Promote to the next unit when rounding to one decimal would otherwise
    // display at the boundary, e.g. 1048575 as "1024.0 KiB" instead of
    // "1.0 MiB".
    if val >= 1023.95 && exp < UNITS.len() - 1 {
        div *= UNIT;
        exp += 1;
        val = n as f64 / div as f64;
    }
    format!("{val:.1} {}iB", UNITS[exp])
}

/// Formats a `SystemTime` the way Go renders a local-zone `time.Time` with
/// `Format(time.RFC3339)`: seconds precision, numeric UTC offset, "Z" when
/// the offset is zero (Go: the pack `Sealed` mtimes and the cycle `Start`).
#[allow(clippy::unnecessary_cast)] // tm_gmtoff is i32 on some targets
fn rfc3339_local(t: SystemTime) -> String {
    let secs = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    };
    let tm = local_tm(secs);
    let base = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        i64::from(tm.tm_year) + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    );
    let off = tm.tm_gmtoff as i64;
    if off == 0 {
        return base + "Z";
    }
    let (sign, off) = if off < 0 { ('-', -off) } else { ('+', off) };
    format!("{base}{sign}{:02}:{:02}", off / 3600, (off % 3600) / 60)
}

// ---------------------------------------------------------------------------
// Go-compatible durations (Go: time.ParseDuration, Duration.String and
// Duration.Round, ported so the gc flags parse and the cycle report prints
// exactly like the Go CLI).
// ---------------------------------------------------------------------------

/// Parses a Go duration string into nanoseconds: decimal numbers, each with
/// an optional fraction and a mandatory unit suffix (ns, us/µs/μs, ms, s,
/// m, h), concatenated like "1m30s"; a leading sign is allowed and bare
/// numbers other than "0" are rejected (Go: `time.ParseDuration`). Used as
/// a clap value parser, so the error side is a plain string; the texts
/// match Go's.
fn parse_go_duration(orig: &str) -> Result<i64, String> {
    let invalid = || format!("time: invalid duration {orig:?}");
    let mut s = orig;
    let mut d: u64 = 0;
    // Consume [-+]?
    let mut neg = false;
    if let Some(&c) = s.as_bytes().first()
        && (c == b'-' || c == b'+')
    {
        neg = c == b'-';
        s = &s[1..];
    }
    // Special case: if all that is left is "0", this is zero.
    if s == "0" {
        return Ok(0);
    }
    if s.is_empty() {
        return Err(invalid());
    }
    while !s.is_empty() {
        // The next character must be [0-9.]
        let c0 = s.as_bytes()[0];
        if !(c0 == b'.' || c0.is_ascii_digit()) {
            return Err(invalid());
        }
        // Consume [0-9]*
        let pl = s.len();
        let (mut v, rest) = leading_int(s).map_err(|()| invalid())?;
        s = rest;
        let pre = pl != s.len(); // whether we consumed anything before a period
        // Consume (\.[0-9]*)?
        let mut post = false;
        let mut f: u64 = 0;
        let mut scale: f64 = 1.0;
        if !s.is_empty() && s.as_bytes()[0] == b'.' {
            s = &s[1..];
            let pl = s.len();
            (f, scale, s) = leading_fraction(s);
            post = pl != s.len();
        }
        if !pre && !post {
            // no digits (e.g. ".s" or "-.s")
            return Err(invalid());
        }
        // Consume unit. The scan is over bytes, but a split can only land
        // on an ASCII digit or '.', which is always a char boundary.
        let mut i = 0;
        for &c in s.as_bytes() {
            if c == b'.' || c.is_ascii_digit() {
                break;
            }
            i += 1;
        }
        if i == 0 {
            return Err(format!("time: missing unit in duration {orig:?}"));
        }
        let (u, rest) = s.split_at(i);
        s = rest;
        let unit: u64 = match u {
            "ns" => 1,
            // U+00B5 (micro sign) and U+03BC (Greek small mu), as in Go.
            "us" | "\u{00b5}s" | "\u{03bc}s" => 1_000,
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            "m" => 60_000_000_000,
            "h" => 3_600_000_000_000,
            _ => return Err(format!("time: unknown unit {u:?} in duration {orig:?}")),
        };
        if v > (1 << 63) / unit {
            // overflow
            return Err(invalid());
        }
        v *= unit;
        if f > 0 {
            // f64 is needed to be nanosecond-accurate for fractions of
            // hours; v >= 0 && (f*unit/scale) <= 3.6e12 (ns/h, h is the
            // largest unit).
            v += (f as f64 * (unit as f64 / scale)) as u64;
            if v > 1 << 63 {
                return Err(invalid());
            }
        }
        d += v;
        if d > 1 << 63 {
            return Err(invalid());
        }
    }
    if neg {
        // d <= 1<<63 here, so the negation is always representable
        // (i64::MIN when d is exactly 1<<63).
        return Ok(d.wrapping_neg() as i64);
    }
    if d > (1 << 63) - 1 {
        return Err(invalid());
    }
    Ok(d as i64)
}

/// Consumes the leading `[0-9]*` of `s`; `Err` on overflow past `1<<63`
/// (Go: `leadingInt`).
fn leading_int(s: &str) -> Result<(u64, &str), ()> {
    let mut x: u64 = 0;
    let mut i = 0;
    for &c in s.as_bytes() {
        if !c.is_ascii_digit() {
            break;
        }
        if x > (1 << 63) / 10 {
            return Err(());
        }
        x = x * 10 + u64::from(c - b'0');
        if x > 1 << 63 {
            return Err(());
        }
        i += 1;
    }
    Ok((x, &s[i..]))
}

/// Consumes the leading `[0-9]*` of `s` as the value and scale of a decimal
/// fraction; digits past the point of overflow are consumed but ignored
/// (Go: `leadingFraction`).
fn leading_fraction(s: &str) -> (u64, f64, &str) {
    let mut x: u64 = 0;
    let mut scale: f64 = 1.0;
    let mut overflow = false;
    let mut i = 0;
    for &c in s.as_bytes() {
        if !c.is_ascii_digit() {
            break;
        }
        i += 1;
        if overflow {
            continue;
        }
        if x > ((1u64 << 63) - 1) / 10 {
            // It's possible for overflow to give a positive number, so
            // take care.
            overflow = true;
            continue;
        }
        let y = x * 10 + u64::from(c - b'0');
        if y > 1 << 63 {
            overflow = true;
            continue;
        }
        x = y;
        scale *= 10.0;
    }
    (x, scale, &s[i..])
}

/// A std `Duration` as Go `time.Duration` nanoseconds, saturating at
/// `i64::MAX` (~292 years).
fn go_ns(d: Duration) -> i64 {
    i64::try_from(d.as_nanos()).unwrap_or(i64::MAX)
}

/// Rounds `d` nanoseconds to the nearest millisecond, half away from zero,
/// saturating like Go on overflow (Go: `Duration.Round(time.Millisecond)`).
fn round_ms(d: i64) -> i64 {
    const M: i64 = 1_000_000;
    let r = d % M;
    if d < 0 {
        let r = -r;
        if r + r < M {
            return d + r;
        }
        match d.checked_sub(M - r) {
            Some(d1) if d1 < d => d1,
            _ => i64::MIN,
        }
    } else {
        if r + r < M {
            return d - r;
        }
        match d.checked_add(M - r) {
            Some(d1) if d1 > d => d1,
            _ => i64::MAX,
        }
    }
}

/// Formats `d` nanoseconds the way Go's `Duration.String` does: "0s",
/// sub-second values with a single unit (ns/µs/ms), larger values as
/// `[h][m]s` with up to nine fractional digits and trailing zeros dropped —
/// e.g. "1.234s", "12ms", "1m3.5s". The CLI only feeds it the non-negative
/// millisecond-rounded cycle durations, but the full algorithm is ported.
fn format_go_duration(d: i64) -> String {
    // Like Go, the digits fill a fixed buffer from the end.
    let mut buf = [0u8; 32];
    let mut w = buf.len();
    let neg = d < 0;
    let mut u = d.unsigned_abs();
    if u < 1_000_000_000 {
        // Special case: if duration is smaller than a second, use smaller
        // units, like 1.2ms.
        if u == 0 {
            return "0s".to_string();
        }
        let prec;
        w -= 1;
        buf[w] = b's';
        if u < 1_000 {
            // print nanoseconds
            prec = 0;
            w -= 1;
            buf[w] = b'n';
        } else if u < 1_000_000 {
            // print microseconds; U+00B5 'µ' (micro sign) is two bytes
            prec = 3;
            w -= 2;
            buf[w..w + 2].copy_from_slice("\u{00b5}".as_bytes());
        } else {
            // print milliseconds
            prec = 6;
            w -= 1;
            buf[w] = b'm';
        }
        (w, u) = fmt_frac(&mut buf, w, u, prec);
        w = fmt_int(&mut buf, w, u);
    } else {
        w -= 1;
        buf[w] = b's';
        (w, u) = fmt_frac(&mut buf, w, u, 9);
        // u is now integer seconds
        w = fmt_int(&mut buf, w, u % 60);
        u /= 60;
        // u is now integer minutes
        if u > 0 {
            w -= 1;
            buf[w] = b'm';
            w = fmt_int(&mut buf, w, u % 60);
            u /= 60;
            // u is now integer hours; stop there because days can differ
            // in length
            if u > 0 {
                w -= 1;
                buf[w] = b'h';
                w = fmt_int(&mut buf, w, u);
            }
        }
    }
    if neg {
        w -= 1;
        buf[w] = b'-';
    }
    String::from_utf8_lossy(&buf[w..]).into_owned()
}

/// Writes the `prec`-digit fraction of `v` before position `w` in `buf`,
/// omitting trailing zeros and the decimal point if every digit is zero;
/// returns the new write position and `v / 10^prec` (Go: `fmtFrac`).
fn fmt_frac(buf: &mut [u8; 32], mut w: usize, mut v: u64, prec: u32) -> (usize, u64) {
    let mut print = false;
    for _ in 0..prec {
        let digit = v % 10;
        print = print || digit != 0;
        if print {
            w -= 1;
            buf[w] = b'0' + digit as u8;
        }
        v /= 10;
    }
    if print {
        w -= 1;
        buf[w] = b'.';
    }
    (w, v)
}

/// Writes the decimal form of `v` before position `w` in `buf` and returns
/// the new write position (Go: `fmtInt`).
fn fmt_int(buf: &mut [u8; 32], mut w: usize, mut v: u64) -> usize {
    if v == 0 {
        w -= 1;
        buf[w] = b'0';
    } else {
        while v > 0 {
            w -= 1;
            buf[w] = b'0' + (v % 10) as u8;
            v /= 10;
        }
    }
    w
}
