//! Dev-only mirror of the Go `cmd/amber-store` CLI (no progress UI), for
//! interop testing against the Go binary: same store layout
//! (`<dir>/packstore` + `<dir>/refs`), same spec addressing
//! (`KEY[/PATH]` | `ref:NAME[@PATH]`), same subcommand behavior and
//! `ls -l`-style output. A commit key, or a reference to one, stands for the
//! commit's tree wherever a directory is expected, and so does a directory
//! entry that holds a commit.

use std::fs;
use std::io::{self, Seek as _, SeekFrom, Write as _};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Args, Parser, Subcommand};

use amber_store_core::chunkers::ByteOpts;
use amber_store_core::commit::{Commit, Identity};
use amber_store_core::fstree::{self, Entry};
use amber_store_core::gc;
use amber_store_core::ingest;
use amber_store_core::key::{Key, Type};
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
    /// create and inspect commits: a tree with its parent commits, author,
    /// committer and message
    #[command(subcommand)]
    Commit(CommitCmd),
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
    Set {
        /// only if NAME currently points at OLD; 'none': only if NAME does
        /// not exist
        #[arg(long, value_name = "OLD")]
        expect: Option<String>,
        /// NAME KEY. Flags go before them, as in Go (urfave/cli): whatever
        /// follows the first positional is a positional, so a misplaced
        /// --expect fails the argument count instead of being honoured.
        /// A NAME that begins with a dash goes after `--`, as in Go.
        #[arg(value_name = "NAME KEY", trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// delete reference NAME
    Rm {
        /// only if NAME currently points at OLD
        #[arg(long, value_name = "OLD")]
        expect: Option<String>,
        /// NAME; flags go before it
        #[arg(value_name = "NAME", trailing_var_arg = true)]
        args: Vec<String>,
    },
}

#[derive(Subcommand)]
enum CommitCmd {
    /// record the directory at TREE as a commit and print the commit key
    Create(CommitCreateArgs),
    /// print the commit at KEY or ref:NAME
    Show {
        #[arg(value_name = "KEY | ref:NAME")]
        spec: String,
    },
}

#[derive(Args)]
struct CommitCreateArgs {
    /// commit message
    #[arg(long, short = 'm', default_value = "")]
    message: String,
    /// author as 'Name <email>'
    #[arg(long)]
    author: String,
    /// committer as 'Name <email>' (default: the author)
    #[arg(long)]
    committer: Option<String>,
    /// author and committer time, RFC 3339 (default: now, local zone)
    #[arg(long)]
    date: Option<String>,
    /// parent commit, KEY or ref:NAME; repeat for a merge, mainline first
    #[arg(long)]
    parent: Vec<String>,
    /// change id as HEX, 1-64 bytes: an identity that follows the change when
    /// the commit is rewritten
    #[arg(long = "change-id", value_name = "HEX")]
    change_id: Option<String>,
    /// point reference NAME at the new commit
    #[arg(long = "ref")]
    reference: Option<String>,
    /// the directory to record: KEY[/PATH] | ref:NAME[@PATH]
    #[arg(value_name = "TREE")]
    tree: String,
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
        Cmd::Commit(c) => run_commit(&cli, c),
        Cmd::Gc(g) => run_gc(&cli, g),
    }
}

// ---------------------------------------------------------------------------
// Store plumbing (Go: store.go).
// ---------------------------------------------------------------------------

struct Stores {
    // Arcs so a collector can share the open stores (Go passes the same
    // pointers).
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
/// references DB. Any number of processes may have one store open at once
/// (architecture/packstore.md, architecture/references.md).
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

/// What a reference put needs from the collector: the completeness walk under
/// the reference lock. A collector opens a span for the one put; a
/// [`gc::Span`] is a span already open around the writes the reference names
/// (Go: `refGate`).
trait RefGate {
    fn prepare_ref(&self, root: Key) -> Result<gc::PreparedRef<'_>, gc::Error>;
    fn release_ref(&self, root: Key) -> Result<(), gc::Error>;
}

impl RefGate for gc::Collector {
    fn prepare_ref(&self, root: Key) -> Result<gc::PreparedRef<'_>, gc::Error> {
        gc::Collector::prepare_ref(self, root)
    }

    fn release_ref(&self, root: Key) -> Result<(), gc::Error> {
        gc::Collector::release_ref(self, root)
    }
}

impl RefGate for gc::Span<'_> {
    fn prepare_ref(&self, root: Key) -> Result<gc::PreparedRef<'_>, gc::Error> {
        gc::Span::prepare_ref(self, root)
    }

    fn release_ref(&self, root: Key) -> Result<(), gc::Error> {
        gc::Span::release_ref(self, root)
    }
}

/// One write span over a command's object writes and, when the command names
/// them, the reference put. With a reference to put it is the collector's
/// ([`gc::Collector::begin_span`]), which takes the reference lock before the
/// store's gate, the order a cycle takes them in; without one, the store's
/// own. Dropping it ends the span (Go: `cliSpan`; the caller opens the
/// collector here, because the span borrows it).
enum CliSpan<'a> {
    Store { _span: packstore::WriteSpan<'a> },
    Collector(gc::Span<'a>),
}

impl CliSpan<'_> {
    /// What a reference put inside this span goes through.
    fn ref_gate(&self) -> Result<&dyn RefGate, CliError> {
        match self {
            CliSpan::Collector(span) => Ok(span),
            CliSpan::Store { .. } => Err("a reference put needs the collector's span".into()),
        }
    }
}

/// Go: `openSpan`.
fn open_span<'a>(st: &'a Stores, coll: Option<&'a gc::Collector>) -> Result<CliSpan<'a>, CliError> {
    Ok(match coll {
        Some(coll) => CliSpan::Collector(coll.begin_span()?),
        None => CliSpan::Store {
            _span: st.objects.begin_write()?,
        },
    })
}

/// The collector a command needs when it names what it writes.
fn collector_for(cli: &Cli, st: &Stores, wanted: bool) -> Result<Option<gc::Collector>, CliError> {
    if !wanted {
        return Ok(None);
    }
    open_collector(cli, st, gc::Options::default()).map(Some)
}

/// Closes the collector a span was opened through, if there was one.
fn close_collector(coll: Option<gc::Collector>) -> Result<(), CliError> {
    match coll {
        Some(coll) => coll.close().map_err(CliError::from),
        None => Ok(()),
    }
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
/// entry's content key, as stored. Every traversed segment must be an entry
/// carrying a content key (a regular file or a directory). A Commit, as the
/// root or as the content key of a directory entry on the way, stands for its
/// tree: fstree's readers pass through it. The key returned may itself be a
/// commit's; every reader takes it, and `fstree::dir_of` names its directory.
fn descend(objects: &packstore::Store, root: Key, path: &str) -> Result<Key, CliError> {
    let mut k = root;
    for seg in path.split('/') {
        if seg.is_empty() {
            continue;
        }
        let e = fstree::lookup_entry(k, seg.as_bytes(), |kk| objects.get(kk))
            .map_err(|e| format!("resolving {path:?}: {e}"))?;
        let ck = Key::parse(&e.content_key)
            .map_err(|_| format!("resolving {path:?}: {seg:?} is not a file or directory"))?;
        // The codec does not hold an entry's content key to its mode. A
        // commit under anything but a directory entry is a malformed tree.
        if ck.type_() == Type::Commit && e.mode & S_IFMT != S_IFDIR {
            return Err(format!(
                "resolving {path:?}: {seg:?} holds a commit but is not a directory entry"
            )
            .into());
        }
        k = ck;
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
    // One write span over the ingest and the reference that names it: a GC
    // cycle in another process cannot fall between the two and find objects
    // whose reference is still to come, which only the grace period would
    // protect (Go: runIngest's openSpan).
    let coll = match collector_for(cli, &st, a.reference.is_some()) {
        Ok(coll) => coll,
        Err(e) => {
            let _ = close_store(st);
            return Err(e);
        }
    };
    let res = ingest_in_span(&st, coll.as_ref(), a, opts);
    let closed = close_collector(coll);
    let root = match res {
        Ok(root) => root,
        Err(e) => {
            let _ = close_store(st);
            return Err(e);
        }
    };
    closed?;
    close_store(st)?;
    println!("{root}");
    Ok(())
}

/// The part of an ingest that runs inside its write span.
fn ingest_in_span(
    st: &Stores,
    coll: Option<&gc::Collector>,
    a: &IngestArgs,
    opts: ingest::Opts,
) -> Result<Key, CliError> {
    let span = open_span(st, coll)?;
    let (_stats, res) = ingest::dir(&st.objects, &a.path, opts);
    let root = res?;
    if let Some(name) = &a.reference {
        let rec = Reference {
            name: name.clone(),
            key: root.as_bytes().to_vec(),
            created_at: now_unix_nanos(),
            ..Default::default()
        };
        // The reference is published through the collector, so an incomplete
        // tree can fail the write — shouldn't happen right after ingest.
        let put = rec.encode().map_err(CliError::from).and_then(|raw| {
            put_ref(
                span.ref_gate()?,
                &st.refs,
                name,
                root,
                &raw,
                Expectation::Unconditional,
            )
        });
        if let Err(e) = put {
            return Err(format!(
                "tree stored (root {root}) but creating reference {name:?} failed: {e}\n\
                 retry with: amber-store ref set {name:?} {root}"
            )
            .into());
        }
    }
    Ok(root)
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

/// The precondition of an optimistic reference write: the reference must
/// not have moved since the caller last looked (Go: `expectation`).
#[derive(Clone, Copy)]
enum Expectation {
    /// Write unconditionally.
    Unconditional,
    /// The reference must not exist.
    Absent,
    /// The reference must point here.
    At(Key),
}

/// Parses --expect (Go: `parseExpect`). `given` is `None` when the flag was
/// not passed at all.
fn parse_expect(given: Option<&str>, allow_none: bool) -> Result<Expectation, CliError> {
    let Some(s) = given else {
        return Ok(Expectation::Unconditional);
    };
    match s {
        // A script's unset variable. It must not quietly turn the write
        // into an unconditional one.
        "" => Err("--expect is empty: it takes the key the reference must point at".into()),
        "none" if allow_none => Ok(Expectation::Absent),
        "none" => Err("--expect none: a delete cannot expect the reference to be absent".into()),
        _ => match parse_hex_key(s) {
            Ok(k) => Ok(Expectation::At(k)),
            Err(e) => Err(format!("--expect: {e}").into()),
        },
    }
}

impl Expectation {
    /// Turns the store's typed errors into what the user expected (Go:
    /// `expectation.explain`).
    fn explain(&self, name: &str, err: refstore::Error) -> CliError {
        match self {
            Expectation::Absent if err.is_conflict() => {
                format!("reference {name:?} already exists: {err}").into()
            }
            Expectation::At(k) if err.is_conflict() => {
                format!("reference {name:?} does not point at {k}: {err}").into()
            }
            Expectation::At(k) if err.is_not_found() => {
                format!("reference {name:?} does not exist, expected it at {k}: {err}").into()
            }
            _ => err.into(),
        }
    }
}

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
        RefCmd::Set { expect, args } => {
            if args.len() != 2 {
                return Err(
                    format!("ref set requires NAME KEY arguments, got {}", args.len()).into(),
                );
            }
            let name = &args[0];
            let k = parse_hex_key(&args[1])?;
            let exp = parse_expect(expect.as_deref(), true)?;
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
            let res = put_ref(&coll, &st.refs, name, k, &raw, exp);
            let closed = coll.close().map_err(CliError::from);
            drop(coll); // release the collector's store handles first
            join_errs([res, closed, close_store(st)])
        }
        RefCmd::Rm { expect, args } => {
            if args.len() != 1 {
                return Err(format!(
                    "ref rm requires exactly one NAME argument, got {}",
                    args.len()
                )
                .into());
            }
            let name = &args[0];
            let exp = parse_expect(expect.as_deref(), false)?;
            let st = open_store(cli)?;
            let coll = match open_collector(cli, &st, gc::Options::default()) {
                Ok(c) => c,
                Err(e) => {
                    let _ = close_store(st);
                    return Err(e);
                }
            };
            let res = rm_ref(&coll, &st.refs, name, exp);
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
/// Unconditional calls for one name must be serialized by the caller (the
/// one-shot CLI is); the read-old -> prepare -> put -> release sequence is
/// not atomic against a concurrent writer of the same name. With an
/// expectation the store itself refuses the write if the reference moved
/// meanwhile.
fn put_ref(
    coll: &dyn RefGate,
    refs: &refstore::Store,
    name: &str,
    root: Key,
    raw: &[u8],
    exp: Expectation,
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
    let put = match exp {
        Expectation::Unconditional => refs.put(name, raw),
        Expectation::Absent => refs.create(name, raw),
        Expectation::At(expected) => refs.compare_and_swap(name, expected, raw),
    };
    if let Err(e) = put {
        prepared.abort();
        return Err(exp.explain(name, e));
    }
    prepared.commit();
    // With an expectation, what was overwritten is what the store compared
    // against, whatever the read above saw: the expected key, or nothing.
    let old = match exp {
        Expectation::Unconditional => old,
        Expectation::Absent => None,
        Expectation::At(expected) => Some(expected),
    };
    if let Some(old) = old {
        coll.release_ref(old)?;
    }
    Ok(())
}

/// Deletes a reference and releases its root: the tails leave the union;
/// the closure file goes if no other name shares the root. No walk (Go:
/// `rmRef`).
fn rm_ref(
    coll: &gc::Collector,
    refs: &refstore::Store,
    name: &str,
    exp: Expectation,
) -> Result<(), CliError> {
    let prev = refs.get(name).map_err(|e| exp.explain(name, e))?;
    let rec = Reference::decode(&prev).map_err(|e| format!("reference {name:?}: {e}"))?;
    let root = Key::parse(&rec.key).map_err(|e| format!("reference {name:?}: {e}"))?;
    let (deleted, root) = match exp {
        // What was deleted pointed at the expected key, whatever prev said.
        Expectation::At(expected) => (refs.compare_and_delete(name, expected), expected),
        _ => (refs.delete(name), root),
    };
    deleted.map_err(|e| exp.explain(name, e))?;
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
// commit (Go: commit.go).
// ---------------------------------------------------------------------------

fn run_commit(cli: &Cli, cmd: &CommitCmd) -> Result<(), CliError> {
    match cmd {
        CommitCmd::Create(a) => run_commit_create(cli, a),
        CommitCmd::Show { spec } => run_commit_show(cli, spec),
    }
}

/// Splits "Name <email>" into its parts. A string with no trailing "<...>"
/// is all name; the name must not be empty (Go: `parseIdentity`).
fn parse_identity(s: &str) -> Result<Identity, CliError> {
    let s = s.trim();
    let (mut name, mut email) = (s, "");
    if s.ends_with('>')
        && let Some(i) = s.rfind('<')
    {
        name = s[..i].trim();
        email = &s[i + 1..s.len() - 1];
    }
    if name.is_empty() {
        return Err(format!("identity {s:?} has no name; want 'Name <email>'").into());
    }
    Ok(Identity {
        name: name.to_string(),
        email: email.to_string(),
        ..Default::default()
    })
}

fn run_commit_create(cli: &Cli, a: &CommitCreateArgs) -> Result<(), CliError> {
    let mut author = parse_identity(&a.author).map_err(|e| format!("--author: {e}"))?;
    let mut committer = match a.committer.as_deref() {
        Some(s) if !s.is_empty() => parse_identity(s).map_err(|e| format!("--committer: {e}"))?,
        _ => author.clone(),
    };
    let (when, tz_offset) = match a.date.as_deref() {
        Some(s) if !s.is_empty() => parse_rfc3339(s).map_err(|e| format!("--date: {e}"))?,
        _ => {
            // Now, in the local zone; the zone's offset is recorded (Go:
            // `when.Zone()`, whole minutes).
            let now = now_unix_nanos();
            let tm = local_tm(now.div_euclid(1_000_000_000));
            (now, (tm.tm_gmtoff / 60) as i32)
        }
    };
    (author.when, author.tz_offset) = (when, tz_offset);
    (committer.when, committer.tz_offset) = (when, tz_offset);
    let mut change_id = Vec::new();
    if let Some(h) = a.change_id.as_deref() {
        change_id = go_hex_decode(h).map_err(|e| format!("--change-id: {e}"))?;
        if change_id.is_empty() {
            return Err(
                "--change-id: empty; leave the flag out for a commit without a change id".into(),
            );
        }
    }
    let ref_name = a.reference.as_deref().filter(|n| !n.is_empty());
    if let Some(name) = ref_name {
        reference::validate_name(name)?;
    }

    let st = open_store(cli)?;
    let created = create_commit(cli, &st, a, author, committer, change_id, ref_name);
    let closed = close_store(st);
    let (key, res) = match created {
        Ok(k) => (Some(k), Ok(())),
        Err(e) => (None, Err(e)),
    };
    join_errs([res, closed])?;
    if let Some(k) = key {
        println!("{k}");
    }
    Ok(())
}

/// Resolves the tree and parent specs, stores the commit and, when
/// `ref_name` is set, points that reference at it (Go: `createCommit`).
fn create_commit(
    cli: &Cli,
    st: &Stores,
    a: &CommitCreateArgs,
    author: Identity,
    committer: Identity,
    change_id: Vec<u8>,
    ref_name: Option<&str>,
) -> Result<Key, CliError> {
    let (root, path) = resolve_spec(&st.refs, &a.tree)?;
    let target = descend(&st.objects, root, &path)?;
    // TREE may name a commit, as the root or through a directory entry that
    // holds one: the new commit records that commit's tree.
    let tree = fstree::dir_of(target, |kk| st.objects.get(kk))?;
    let mut parents = Vec::with_capacity(a.parent.len());
    for spec in &a.parent {
        let (pk, ppath) =
            resolve_spec(&st.refs, spec).map_err(|e| format!("--parent {spec}: {e}"))?;
        if !ppath.is_empty() {
            return Err(
                format!("--parent {spec}: a parent is a commit, not a path within one").into(),
            );
        }
        parents.push(pk);
    }
    let rec = Commit {
        tree,
        parents,
        author,
        committer,
        message: a.message.clone(),
        signature: Vec::new(),
        public_key: Vec::new(),
        change_id,
        conflict_terms: Vec::new(),
        conflict_labels: Vec::new(),
    };
    let (k, raw) = rec.object()?;
    // One write span from the check that the children are there to the
    // reference that names the commit: no sweep in another process falls in
    // between (Go: createCommit's openSpan).
    let coll = collector_for(cli, st, ref_name.is_some())?;
    let res = commit_in_span(st, coll.as_ref(), &rec, k, &raw, ref_name);
    let closed = close_collector(coll);
    let k = res?;
    closed?;
    Ok(k)
}

/// The part of a commit's creation that runs inside its write span.
fn commit_in_span(
    st: &Stores,
    coll: Option<&gc::Collector>,
    rec: &Commit,
    k: Key,
    raw: &[u8],
    ref_name: Option<&str>,
) -> Result<Key, CliError> {
    let span = open_span(st, coll)?;
    for child in std::iter::once(rec.tree).chain(rec.parents.iter().copied()) {
        if !st.objects.has(child)? {
            return Err(format!("{child} is not in the store").into());
        }
    }
    // A parent has to be a commit the graph walks accept. Bytes under a key of
    // the first release's rule still decode, but no reference could ever be
    // put on what is built on them.
    for p in &rec.parents {
        let data = st.objects.get(*p)?;
        fstree::child_keys(*p, &data).map_err(|e| format!("parent {p}: {e}"))?;
    }
    st.objects.put(k, raw)?;
    let Some(name) = ref_name else {
        return Ok(k);
    };
    let ref_rec = Reference {
        name: name.to_string(),
        key: k.as_bytes().to_vec(),
        created_at: now_unix_nanos(),
        ..Default::default()
    };
    let put = ref_rec
        .encode()
        .map_err(CliError::from)
        .and_then(|ref_raw| {
            put_ref(
                span.ref_gate()?,
                &st.refs,
                name,
                k,
                &ref_raw,
                Expectation::Unconditional,
            )
        });
    if let Err(e) = put {
        return Err(format!(
            "commit stored ({k}) but setting reference {name:?} failed: {e}\n\
             retry with: amber-store ref set {name:?} {k}"
        )
        .into());
    }
    Ok(k)
}

fn run_commit_show(cli: &Cli, spec: &str) -> Result<(), CliError> {
    let st = open_store(cli)?;
    let res = commit_show_inner(&st, spec);
    let closed = close_store(st);
    join_errs([res, closed])
}

fn commit_show_inner(st: &Stores, spec: &str) -> Result<(), CliError> {
    let (k, path) = resolve_spec(&st.refs, spec)?;
    if !path.is_empty() {
        return Err("commit show takes a commit, not a path within one".into());
    }
    if k.type_() != Type::Commit {
        return Err(format!("{k} is not a commit (type {})", k.type_()).into());
    }
    let data = st.objects.get(k)?;
    let rec = Commit::decode(&data).map_err(|e| format!("commit {k}: {e}"))?;
    io::stdout().write_all(render_commit(k, &rec).as_bytes())?;
    Ok(())
}

/// Renders a commit in git's cat-file layout: headers, a blank line, then
/// the message indented by four spaces (Go: `renderCommit`). A conflicted
/// tree shows its further terms after the tree, in recorded order (remove,
/// add, remove, …), and the labels that are not empty, numbered from 0, the
/// tree.
fn render_commit(k: Key, c: &Commit) -> String {
    let mut b = format!("commit {k}\ntree {}\n", c.tree);
    for (i, term) in c.conflict_terms.iter().enumerate() {
        let side = if i % 2 == 1 { "add" } else { "remove" };
        b.push_str(&format!("conflict-{side} {term}\n"));
    }
    for (i, label) in c.conflict_labels.iter().enumerate() {
        if !label.is_empty() {
            b.push_str(&format!("conflict-label {i} {label}\n"));
        }
    }
    for p in &c.parents {
        b.push_str(&format!("parent {p}\n"));
    }
    if !c.change_id.is_empty() {
        b.push_str(&format!("change-id {}\n", hex::encode(&c.change_id)));
    }
    b.push_str(&format!(
        "author {}\ncommitter {}\n",
        identity_line(&c.author),
        identity_line(&c.committer)
    ));
    if !c.signature.is_empty() {
        b.push_str(&format!("signature {} bytes\n", c.signature.len()));
    }
    let msg = c.message.trim_end_matches('\n');
    if !msg.is_empty() {
        b.push('\n');
        for line in msg.split('\n') {
            b.push_str(&format!("    {line}\n"));
        }
    }
    b
}

/// Renders "Name <email> time", the time in the identity's own zone (Go:
/// `identityLine`).
fn identity_line(id: &Identity) -> String {
    let mut who = id.name.clone();
    if !id.email.is_empty() {
        if !who.is_empty() {
            who.push(' ');
        }
        who.push_str(&format!("<{}>", id.email));
    }
    let when = rfc3339_at(id.when, id.tz_offset);
    if who.is_empty() {
        // an identity may name nobody at all
        return when;
    }
    format!("{who} {when}")
}

/// Decodes a hex string the way Go's `hex.DecodeString` does, its two error
/// texts included: pairs are checked left to right, and a bad character is
/// reported before an odd length.
fn go_hex_decode(s: &str) -> Result<Vec<u8>, String> {
    fn nibble(c: u8) -> Result<u8, String> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            b'A'..=b'F' => Ok(c - b'A' + 10),
            // Go formats the byte with %#U, as the code point of that value:
            // the character is shown when strconv.IsPrint says so.
            0x20..=0x7e | 0xa1..=0xac | 0xae..=0xff => Err(format!(
                "encoding/hex: invalid byte: U+{c:04X} '{}'",
                c as char
            )),
            _ => Err(format!("encoding/hex: invalid byte: U+{c:04X}")),
        }
    }
    let src = s.as_bytes();
    let mut out = Vec::with_capacity(src.len() / 2);
    for pair in src.as_chunks::<2>().0 {
        let hi = nibble(pair[0])?;
        out.push(hi << 4 | nibble(pair[1])?);
    }
    if src.len() % 2 == 1 {
        nibble(src[src.len() - 1])?;
        return Err("encoding/hex: odd length hex string".into());
    }
    Ok(out)
}

/// Formats a ns-precision Unix timestamp at a fixed offset the way Go's
/// `time.Unix(0, ns).In(time.FixedZone("", min*60)).Format(time.RFC3339)`
/// does: seconds precision, "Z" for a zero offset, "±hh:mm" otherwise.
fn rfc3339_at(ns: i64, tz_minutes: i32) -> String {
    let secs = ns.div_euclid(1_000_000_000) + i64::from(tz_minutes) * 60;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let zone = if tz_minutes == 0 {
        "Z".to_string()
    } else {
        let sign = if tz_minutes < 0 { '-' } else { '+' };
        let abs = tz_minutes.unsigned_abs();
        format!("{sign}{:02}:{:02}", abs / 60, abs % 60)
    };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}{zone}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 of a proleptic-Gregorian date (Howard Hinnant's
/// `days_from_civil`), the inverse of [`civil_from_days`].
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400); // [0, 399]
    let mp = (m + 9) % 12; // March = 0
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Parses an RFC 3339 timestamp, `2006-01-02T15:04:05[.frac](Z|±hh:mm)`, into
/// (ns since the Unix epoch, offset in minutes) — what Go's
/// `time.Parse(time.RFC3339, s)` followed by `UnixNano()` and `Zone()` yields.
fn parse_rfc3339(s: &str) -> Result<(i64, i32), String> {
    let bad = || format!("parsing time {s:?} as \"2006-01-02T15:04:05Z07:00\"");
    let num = |from: usize, to: usize| -> Option<i64> {
        let t = s.get(from..to)?;
        if t.is_empty() || !t.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        t.parse().ok()
    };
    let b = s.as_bytes();
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return Err(bad());
    }
    let (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(sec)) = (
        num(0, 4),
        num(5, 7),
        num(8, 10),
        num(11, 13),
        num(14, 16),
        num(17, 19),
    ) else {
        return Err(bad());
    };
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let month_days = match mo {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return Err(format!("{}: month out of range", bad())),
    };
    if d < 1 || d > month_days {
        return Err(format!("{}: day out of range", bad()));
    }
    if h > 23 || mi > 59 || sec > 59 {
        return Err(format!("{}: time out of range", bad()));
    }
    // Optional fractional seconds: at least one digit, kept to ns precision.
    let mut i = 19;
    let mut frac_ns = 0i64;
    if b[i] == b'.' {
        let start = i + 1;
        let mut j = start;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j == start {
            return Err(bad());
        }
        for k in 0..9 {
            let digit = if start + k < j {
                i64::from(b[start + k] - b'0')
            } else {
                0
            };
            frac_ns = frac_ns * 10 + digit;
        }
        i = j;
    }
    let offset_min = match b.get(i) {
        Some(b'Z') if i + 1 == b.len() => 0,
        Some(sign @ (b'+' | b'-')) if b.len() == i + 6 && b[i + 3] == b':' => {
            let (Some(hh), Some(mm)) = (num(i + 1, i + 3), num(i + 4, i + 6)) else {
                return Err(bad());
            };
            if hh > 23 || mm > 59 {
                return Err(format!("{}: time zone offset out of range", bad()));
            }
            let v = hh * 60 + mm;
            if *sign == b'-' { -v } else { v }
        }
        _ => return Err(bad()),
    };
    let secs = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec - offset_min * 60;
    let ns = secs
        .checked_mul(1_000_000_000)
        .and_then(|n| n.checked_add(frac_ns))
        .ok_or_else(|| format!("{}: outside the int64 nanosecond range", bad()))?;
    Ok((ns, offset_min as i32))
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
