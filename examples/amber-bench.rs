//! amber-bench: an ingest → delete → gc benchmark for amber-store (Go:
//! `cmd/amber-bench`).
//!
//! The workload is 1000 references over ~50 GiB of unique data, interleaved
//! into two classes so that deleting 700 of them leaves ~30 GiB: "kept" refs
//! (i%10 < 3) carry 100 MiB of fresh random data each, "deleted" refs
//! (i%10 >= 3) carry 30 MiB. Every ref i > 0 also contains whole-file
//! copies (reflinks, so they cost no disk) of ~1/3 of its fresh size taken
//! from ref i-1, so ~25 % of the ingested bytes are duplicates already in the
//! store — and, because kept refs copy from deleted neighbours too, the
//! bytes the deleted refs "own" and the bytes a collection can actually
//! reclaim differ. Files are 256 KiB–4 MiB; content is seeded, so a rerun
//! produces the identical dataset and pack layout (bit-identical to the Go
//! harness for the same flags — the generator ports Go's `math/rand/v2` PCG
//! verbatim).
//!
//! Phases (--phase, default all): gen writes the dataset; ingest stores every
//! ref in-process the way the CLI and a daemon do (ingest::dir, then the
//! collector's prepare_ref) and times each; delete removes the 700 through
//! release_ref; gc runs the real CLI — `gc run` under policy, then a forced
//! `--garbage 0` pass — with `gc status` and du snapshots around each step;
//! verify runs fstree::check_complete on every surviving ref and restores a
//! sample through the CLI, comparing it byte for byte with the source; report
//! prints the summary from the results file. Each phase appends to --out, so
//! phases can be rerun individually.
//!
//! The dataset plus the store need ~2× the unique size on disk. At full
//! scale the run is a few minutes on an SSD; --refs and --scale shrink it
//! (--refs 30 --scale 0.1 --segment 4194304 is a seconds-long smoke test
//! that still seals and reaps packs).
//!
//! ```text
//! cargo build --release --example amber-store --example amber-bench
//! target/release/examples/amber-bench --data /tmp/bench/data \
//!     --store /tmp/bench/store --restore /tmp/bench/restore \
//!     --bin target/release/examples/amber-store --out /tmp/bench/results.json
//! ```

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read as _, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::Parser;
use serde::{Deserialize, Serialize};

use amber_store_core::chunkers::ByteOpts;
use amber_store_core::fstree;
use amber_store_core::gc;
use amber_store_core::ingest;
use amber_store_core::key::Key;
use amber_store_core::packstore;
use amber_store_core::reference::Reference;
use amber_store_core::refstore;

type BenchError = Box<dyn std::error::Error>;

/// Logs to stderr with an `HH:MM:SS ` prefix (Go: `logf`).
macro_rules! logf {
    ($($arg:tt)*) => {
        eprintln!("{} {}", hhmmss(), format_args!($($arg)*))
    };
}

const MIB: i64 = 1 << 20;
const MIN_FILE: i64 = 256 << 10;
const MAX_FILE: i64 = 4 * MIB;
const SEED: u64 = 20260824;

/// The command line (Go: `config` + the flag set in `main`).
#[derive(Parser, Clone)]
#[command(name = "amber-bench", disable_help_subcommand = true)]
struct Config {
    /// dataset directory (written by gen)
    #[arg(long, default_value = "")]
    data: String,
    /// store directory
    #[arg(long, default_value = "")]
    store: String,
    /// amber-store CLI binary for the gc and verify phases (default:
    /// amber-store in PATH)
    #[arg(long, default_value = "")]
    bin: String,
    /// results file; every phase reads and extends it
    #[arg(long, default_value = "results.json")]
    out: String,
    /// scratch directory for verify's sample restores (empty: skip them)
    #[arg(long, default_value = "")]
    restore: String,
    /// number of references
    #[arg(long, default_value_t = 1000)]
    refs: usize,
    /// multiplier on every per-ref size
    #[arg(long, default_value_t = 1.0)]
    scale: f64,
    /// pack segment size in bytes (harness and CLI)
    #[arg(long, default_value_t = packstore::DEFAULT_SEGMENT_SIZE)]
    segment: u64,
    /// gen|ingest|delete|gc|verify|report|all
    #[arg(long, default_value = "all")]
    phase: String,
    /// reserve space before every write (packstore Options::preallocate), to
    /// measure what that reservation costs
    #[arg(long, default_value_t = false)]
    preallocate: bool,
}

fn main() {
    let mut cfg = Config::parse();
    let phase = cfg.phase.clone();
    if let Err(e) = run(&mut cfg, &phase) {
        eprintln!("amber-bench: {e}");
        std::process::exit(1);
    }
}

fn run(cfg: &mut Config, phase: &str) -> Result<(), BenchError> {
    if phase != "report" && (cfg.data.is_empty() || cfg.store.is_empty()) {
        return Err("--data and --store are required".into());
    }
    if cfg.bin.is_empty()
        && let Some(p) = look_path("amber-store")
    {
        cfg.bin = p;
    }
    let mut res = load_results(&cfg.out)?;
    res.refs = cfg.refs as i64;
    res.scale = cfg.scale;
    match phase {
        "gen" => phase_gen(cfg, &mut res),
        "ingest" => phase_ingest(cfg, &mut res),
        "delete" => phase_delete(cfg, &mut res),
        "gc" => phase_gc(cfg, &mut res),
        "verify" => phase_verify(cfg, &mut res),
        "report" => report(&mut io::stdout(), &res),
        "all" => {
            phase_gen(cfg, &mut res)?;
            phase_ingest(cfg, &mut res)?;
            snapshot_store(cfg, &mut res, "after-ingest")?;
            phase_delete(cfg, &mut res)?;
            snapshot_store(cfg, &mut res, "after-delete")?;
            phase_gc(cfg, &mut res)?;
            phase_verify(cfg, &mut res)?;
            report(&mut io::stdout(), &res)
        }
        _ => Err(format!("unknown phase {phase:?}").into()),
    }
}

// ---------------------------------------------------------------------------
// Results (Go: the results structs + loadResults/saveResults). Serialized
// field names are byte-identical to Go's default JSON names; the writer uses
// Go MarshalIndent's one-space indent.
// ---------------------------------------------------------------------------

/// Deserializes Go's `null` for a nil slice as the empty vector.
fn null_vec<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(d)?.unwrap_or_default())
}

/// One generated file (Go: `fileInfo`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct FileInfo {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Size")]
    size: i64,
}

/// What gen wrote for one ref (Go: `refManifest`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct RefManifest {
    #[serde(rename = "Index")]
    index: i64,
    #[serde(rename = "Kept")]
    kept: bool,
    /// Bytes of fresh random data.
    #[serde(rename = "Fresh")]
    fresh: i64,
    /// Bytes cloned from the previous ref.
    #[serde(rename = "Shared")]
    shared: i64,
    /// Fresh + Shared: what ingest reads.
    #[serde(rename = "Logical")]
    logical: i64,
    #[serde(rename = "Files", deserialize_with = "null_vec")]
    files: Vec<FileInfo>,
}

/// One ref's ingest timings and stats (Go: `refIngest`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct RefIngest {
    #[serde(rename = "Index")]
    index: i64,
    #[serde(rename = "Kept")]
    kept: bool,
    #[serde(rename = "Logical")]
    logical: i64,
    #[serde(rename = "Fresh")]
    fresh: i64,
    #[serde(rename = "Shared")]
    shared: i64,
    #[serde(rename = "Stored")]
    stored: i64,
    #[serde(rename = "Deduped")]
    deduped: i64,
    #[serde(rename = "BytesStored")]
    bytes_stored: i64,
    /// ingest::dir.
    #[serde(rename = "IngestNs")]
    ingest_ns: i64,
    /// prepare_ref walk + refstore put + commit.
    #[serde(rename = "RefNs")]
    ref_ns: i64,
}

/// A disk-usage snapshot of the store (Go: `snapshot`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Snapshot {
    #[serde(rename = "Label")]
    label: String,
    #[serde(rename = "PackstoreKiB")]
    packstore_kib: i64,
    #[serde(rename = "RefsKiB")]
    refs_kib: i64,
    #[serde(rename = "ClosuresKiB")]
    closures_kib: i64,
    /// Sealed + active.
    #[serde(rename = "Segments")]
    segments: i64,
    #[serde(rename = "FreeBytes")]
    free_bytes: u64,
    /// Totals lines of `gc status`.
    #[serde(rename = "GCStatus")]
    gc_status: String,
}

/// One CLI invocation's record (Go: `cliRun`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct CliRun {
    #[serde(rename = "Args", deserialize_with = "null_vec")]
    args: Vec<String>,
    #[serde(rename = "Output")]
    output: String,
    #[serde(rename = "WallNs")]
    wall_ns: i64,
    #[serde(rename = "ExitErr")]
    exit_err: String,
}

/// The whole results file; every phase rewrites it (Go: `results`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Results {
    #[serde(rename = "Refs")]
    refs: i64,
    #[serde(rename = "Scale")]
    scale: f64,
    #[serde(rename = "GenNs")]
    gen_ns: i64,
    #[serde(
        rename = "Manifests",
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "null_vec"
    )]
    manifests: Vec<RefManifest>,
    #[serde(rename = "Ingest", deserialize_with = "null_vec")]
    ingest: Vec<RefIngest>,
    /// First ingest::dir start → last reference commit.
    #[serde(rename = "IngestWallNs")]
    ingest_wall_ns: i64,
    #[serde(rename = "IngestCloseNs")]
    ingest_close_ns: i64,
    #[serde(rename = "DeleteNs")]
    delete_ns: i64,
    #[serde(rename = "DeleteN")]
    delete_n: i64,
    #[serde(rename = "Snapshots", deserialize_with = "null_vec")]
    snapshots: Vec<Snapshot>,
    #[serde(rename = "GCRuns", deserialize_with = "null_vec")]
    gc_runs: Vec<CliRun>,
    #[serde(rename = "VerifyComplete")]
    verify_complete: i64,
    #[serde(rename = "VerifyRestoreOK", deserialize_with = "null_vec")]
    verify_restore_ok: Vec<String>,
    #[serde(rename = "VerifyErrors", deserialize_with = "null_vec")]
    verify_errors: Vec<String>,
}

/// Reads the results file; a missing file is the zero value (Go:
/// `loadResults`).
fn load_results(path: &str) -> Result<Results, BenchError> {
    let b = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Results::default()),
        Err(e) => return Err(e.into()),
    };
    serde_json::from_slice(&b).map_err(|e| format!("{path}: {e}").into())
}

/// Writes the whole results file with Go MarshalIndent's one-space indent
/// (Go: `saveResults`).
fn save_results(cfg: &Config, res: &Results) -> Result<(), BenchError> {
    let mut b = Vec::new();
    let fmt = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut ser = serde_json::Serializer::with_formatter(&mut b, fmt);
    res.serialize(&mut ser)?;
    fs::write(&cfg.out, b)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Small helpers (Go: kept, refName, refDir, freshTarget, human, logf).
// ---------------------------------------------------------------------------

fn kept(i: usize) -> bool {
    i % 10 < 3
}

fn ref_name(i: usize) -> String {
    format!("bench/ref-{i:04}")
}

fn ref_dir(cfg: &Config, i: usize) -> PathBuf {
    Path::new(&cfg.data).join(format!("ref-{i:04}"))
}

impl Config {
    /// Per-ref fresh-data target: the float multiply then truncation is
    /// exactly Go's `int64(100 * MiB * cfg.scale)` (Go: `freshTarget`).
    fn fresh_target(&self, i: usize) -> i64 {
        if kept(i) {
            ((100 * MIB) as f64 * self.scale) as i64
        } else {
            ((30 * MIB) as f64 * self.scale) as i64
        }
    }
}

/// Renders n in binary units, `%.2f XiB` (Go: `human`).
fn human(n: i64) -> String {
    const UNIT: i64 = 1024;
    if n < UNIT {
        return format!("{n} B");
    }
    let (mut div, mut exp) = (UNIT, 0usize);
    let mut m = n / UNIT;
    while m >= UNIT {
        div *= UNIT;
        exp += 1;
        m /= UNIT;
    }
    format!("{:.2} {}iB", n as f64 / div as f64, b"KMGTPE"[exp] as char)
}

/// Local wall-clock `HH:MM:SS` for the log prefix (Go:
/// `time.Now().Format("15:04:05")`).
fn hhmmss() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs()) as i64;
    let t = secs as libc::time_t;
    // SAFETY: localtime_r fills the out-param and touches nothing else.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    };
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

fn ns(d: Duration) -> i64 {
    d.as_nanos() as i64
}

/// Rounds to milliseconds for log lines, like Go's
/// `Duration.Round(time.Millisecond)` before printing.
fn fmt_ms(d: Duration) -> String {
    format!(
        "{:?}",
        Duration::from_millis((d.as_secs_f64() * 1000.0).round() as u64)
    )
}

/// Rounds to whole seconds for log lines, like Go's
/// `Duration.Round(time.Second)` before printing.
fn fmt_s(d: Duration) -> String {
    format!("{}s", d.as_secs_f64().round() as u64)
}

fn available_parallelism() -> usize {
    thread::available_parallelism().map_or(1, |n| n.get())
}

/// Resolves `name` against `$PATH` (Go: `exec.LookPath`, the `-bin`
/// default). Only executable regular files count.
fn look_path(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        // An empty $PATH element means the current directory, as in Go.
        let dir = if dir.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            dir
        };
        let cand = dir.join(name);
        if let Ok(md) = cand.metadata()
            && md.is_file()
            && is_executable(&md)
        {
            return Some(cand.to_string_lossy().into_owned());
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(md: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    md.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_md: &fs::Metadata) -> bool {
    true
}

// ---------------------------------------------------------------------------
// Go math/rand/v2 PCG (Go: pcg.go + rand.go in $GOROOT/src/math/rand/v2).
// Ported verbatim so gen produces the bit-identical dataset for the same
// flags: 128-bit LCG state advance with the DXSM output permutation computed
// on the POST-advance state. Do not substitute a crate: numpy-style
// pcg64dxsm outputs from the pre-advance state with a different multiplier.
// ---------------------------------------------------------------------------

/// Go's `rand.PCG` + the `rand.Rand` draws this harness consumes.
struct Pcg {
    hi: u64,
    lo: u64,
}

impl Pcg {
    /// Seeds the 128-bit state directly: `hi = seed1`, `lo = seed2` — no
    /// scrambling (Go: `rand.NewPCG`).
    fn new(seed1: u64, seed2: u64) -> Pcg {
        Pcg {
            hi: seed1,
            lo: seed2,
        }
    }

    /// Advances the 128-bit LCG state and returns it (Go: `(*PCG).next`).
    fn next(&mut self) -> (u64, u64) {
        const MUL_HI: u64 = 2549297995355413924;
        const MUL_LO: u64 = 4865540595714422341;
        const INC_HI: u64 = 6364136223846793005;
        const INC_LO: u64 = 1442695040888963407;
        // state = state*mul + inc, 128-bit.
        let wide = (self.lo as u128) * (MUL_LO as u128);
        let mut hi = (wide >> 64) as u64;
        let lo = wide as u64;
        hi = hi
            .wrapping_add(self.hi.wrapping_mul(MUL_LO))
            .wrapping_add(self.lo.wrapping_mul(MUL_HI));
        let (lo, carry) = lo.overflowing_add(INC_LO);
        hi = hi.wrapping_add(INC_HI).wrapping_add(carry as u64);
        self.lo = lo;
        self.hi = hi;
        (hi, lo)
    }

    /// PCG-DXSM output on the newly-advanced state (Go: `(*PCG).Uint64`).
    fn uint64(&mut self) -> u64 {
        const CHEAP_MUL: u64 = 0xda942042e4dd58b5;
        let (mut hi, lo) = self.next();
        hi ^= hi >> 32;
        hi = hi.wrapping_mul(CHEAP_MUL);
        hi ^= hi >> 48;
        hi = hi.wrapping_mul(lo | 1);
        hi
    }

    /// Uniform draw in `[0, n)`: a masked single draw when n is a power of
    /// two, else Lemire with the `lo < n` pre-check (Go: `(*Rand).uint64n`;
    /// the 32-bit path is dead on 64-bit targets and sequence-identical).
    fn uint64n(&mut self, n: u64) -> u64 {
        if n & n.wrapping_sub(1) == 0 {
            // n is a power of two, can mask
            return self.uint64() & n.wrapping_sub(1);
        }
        let mut wide = (self.uint64() as u128) * (n as u128);
        let mut lo = wide as u64;
        if lo < n {
            let thresh = n.wrapping_neg() % n;
            while lo < thresh {
                wide = (self.uint64() as u128) * (n as u128);
                lo = wide as u64;
            }
        }
        (wide >> 64) as u64
    }

    /// (Go: `(*Rand).IntN`; n must be positive.)
    fn int_n(&mut self, n: i64) -> i64 {
        assert!(n > 0, "invalid argument to IntN");
        self.uint64n(n as u64) as i64
    }

    /// Top-down Fisher–Yates with `j = uint64n(i+1)` (Go: `(*Rand).Shuffle`).
    fn shuffle<T>(&mut self, s: &mut [T]) {
        for i in (1..s.len()).rev() {
            let j = self.uint64n(i as u64 + 1) as usize;
            s.swap(i, j);
        }
    }
}

// ---------------------------------------------------------------------------
// gen (Go: phaseGen, genFresh, writeRandom, cloneFile, copyFile).
// ---------------------------------------------------------------------------

/// A gen worker's per-ref result slot: the manifest plus the error, filled
/// exactly once (Go: `ms[i], errs[i]`).
type GenSlot = Mutex<Option<(RefManifest, Option<io::Error>)>>;

fn phase_gen(cfg: &Config, res: &mut Results) -> Result<(), BenchError> {
    let n = cfg.refs;
    logf!("gen: {} refs into {}", n, cfg.data);
    fs::create_dir_all(&cfg.data)?;
    let start = Instant::now();
    // Fresh generation in parallel: one worker per core, each with its own
    // reused 1 MiB buffer, pulling ref indices from a rendezvous channel
    // (assignment order nondeterministic, as in Go).
    let slots: Vec<GenSlot> = (0..n).map(|_| Mutex::new(None)).collect();
    let (tx, rx) = mpsc::sync_channel::<usize>(0);
    let rx = Mutex::new(rx);
    thread::scope(|s| {
        let (rx, slots) = (&rx, &slots);
        for _ in 0..available_parallelism() {
            s.spawn(move || {
                let mut buf = vec![0u8; MIB as usize];
                loop {
                    let i = match rx.lock().unwrap().recv() {
                        Ok(i) => i,
                        Err(_) => return,
                    };
                    let out = gen_fresh(cfg, i, &mut buf);
                    *slots[i].lock().unwrap() = Some(out);
                }
            });
        }
        for i in 0..n {
            let _ = tx.send(i);
        }
        drop(tx);
    });
    let mut ms = Vec::with_capacity(n);
    let mut errs = Vec::new();
    for slot in slots {
        let (m, err) = slot
            .into_inner()
            .unwrap()
            .expect("every gen slot is filled");
        if let Some(e) = err {
            errs.push(e.to_string());
        }
        ms.push(m);
    }
    if !errs.is_empty() {
        return Err(errs.join("\n").into());
    }
    logf!(
        "gen: fresh data written in {}; cloning shared files",
        fmt_s(start.elapsed())
    );
    // Shared files: whole-file clones from the previous ref, chosen in a
    // seeded shuffle, never overshooting the target. Ref i-1's manifest
    // already includes its own clones, so sharing is transitive.
    for i in 1..n {
        let mut rng = Pcg::new(SEED + 1, i as u64);
        let target = ms[i].fresh / 3;
        let mut prev = ms[i - 1].files.clone();
        rng.shuffle(&mut prev);
        let mut sum = 0i64;
        for (c, f) in prev.iter().enumerate() {
            if sum + f.size > target {
                continue; // skip and keep scanning — never overshoot
            }
            let src = ref_dir(cfg, i - 1).join(&f.name);
            let name = format!("c{c:04}.bin");
            if let Err(e) = clone_file(&src, &ref_dir(cfg, i).join(&name)) {
                return Err(format!("clone {}: {e}", src.display()).into());
            }
            ms[i].files.push(FileInfo { name, size: f.size });
            sum += f.size;
        }
        ms[i].shared = sum;
    }
    let (mut fresh, mut shared) = (0i64, 0i64);
    for m in &mut ms {
        m.logical = m.fresh + m.shared;
        fresh += m.fresh;
        shared += m.shared;
    }
    res.gen_ns = ns(start.elapsed());
    res.manifests = ms;
    logf!(
        "gen: done in {}: fresh {}, shared {}, logical {} (overlap {:.1}%)",
        fmt_s(start.elapsed()),
        human(fresh),
        human(shared),
        human(fresh + shared),
        100.0 * shared as f64 / (fresh + shared) as f64
    );
    save_results(cfg, res)
}

/// Writes ref i's fresh files: file sizes come from `PCG(seed, i)` — exactly
/// one `IntN(maxFile-minFile+1)` per file, in file order — and file content
/// from a per-file xorshift64* stream (Go: `genFresh`).
fn gen_fresh(cfg: &Config, i: usize, buf: &mut [u8]) -> (RefManifest, Option<io::Error>) {
    let mut rng = Pcg::new(SEED, i as u64);
    let dir = ref_dir(cfg, i);
    if let Err(e) = fs::create_dir_all(&dir) {
        return (RefManifest::default(), Some(e));
    }
    let mut m = RefManifest {
        index: i as i64,
        kept: kept(i),
        ..Default::default()
    };
    let mut remaining = cfg.fresh_target(i);
    let mut n = 0usize;
    while remaining > 0 {
        let size = (MIN_FILE + rng.int_n(MAX_FILE - MIN_FILE + 1)).min(remaining);
        let name = format!("f{n:04}.bin");
        if let Err(e) = write_random(&dir.join(&name), size, (i as u64) << 32 | n as u64, buf) {
            return (m, Some(e));
        }
        m.files.push(FileInfo { name, size });
        m.fresh += size;
        remaining -= size;
        n += 1;
    }
    (m, None)
}

/// Fills path with size bytes of an xorshift64* stream seeded by s:
/// incompressible, so stored bytes track ingested bytes (Go: `writeRandom`).
/// Only full 8-byte words are freshly written per chunk; the final `size%8`
/// tail bytes are deliberately left as stale buffer content, exactly like
/// Go — the buffer is reused across files by one worker.
fn write_random(path: &Path, mut size: i64, s: u64, buf: &mut [u8]) -> io::Result<()> {
    let mut f = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    let mut x = s.wrapping_add(1).wrapping_mul(0x9E3779B97F4A7C15) | 1;
    while size > 0 {
        let n = (buf.len() as i64).min(size) as usize;
        let mut off = 0;
        while off + 8 <= n {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            buf[off..off + 8].copy_from_slice(&x.wrapping_mul(0x2545F4914F6CDD1D).to_le_bytes());
            off += 8;
        }
        f.write_all(&buf[..n])?;
        size -= n as i64;
    }
    Ok(())
}

/// The reflink fallback: a plain byte copy (Go: `copyFile`).
#[cfg(not(target_os = "macos"))]
fn copy_file(src: &Path, dst: &Path) -> io::Result<()> {
    let mut input = fs::File::open(src)?;
    let mut out = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(dst)?;
    io::copy(&mut input, &mut out)?;
    Ok(())
}

/// Makes dst a copy-on-write clone of src (APFS clonefile): instant, no
/// extra blocks, and read back like any other file. Errors surface — no
/// fallback (Go: `cloneFile`, clone_darwin.go).
#[cfg(target_os = "macos")]
fn clone_file(src: &Path, dst: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let s = std::ffi::CString::new(src.as_os_str().as_bytes())?;
    let d = std::ffi::CString::new(dst.as_os_str().as_bytes())?;
    // SAFETY: both arguments are valid NUL-terminated C strings that outlive
    // the call; flags 0 requests a plain clone and clonefile touches nothing
    // else.
    if unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Makes dst a reflink of src (FICLONE: btrfs, xfs, bcachefs) and falls
/// back to a plain copy where the filesystem cannot (Go: `cloneFile`,
/// clone_linux.go).
#[cfg(target_os = "linux")]
fn clone_file(src: &Path, dst: &Path) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let input = fs::File::open(src)?;
    let out = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(dst)?;
    // SAFETY: both descriptors are open and owned for the duration of the
    // call; FICLONE reads src's fd and clones into dst's.
    if unsafe { libc::ioctl(out.as_raw_fd(), libc::FICLONE, input.as_raw_fd()) } == 0 {
        return Ok(());
    }
    drop(out);
    copy_file(src, dst)
}

/// This platform has no reflink primitive (Go: `cloneFile`, clone_other.go).
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn clone_file(src: &Path, dst: &Path) -> io::Result<()> {
    copy_file(src, dst)
}

// ---------------------------------------------------------------------------
// Store plumbing (Go: openAll, closeAll, putRef, rmRef, rootOf).
// ---------------------------------------------------------------------------

/// Opens the store exactly as the amber-store CLI does, plus the collector
/// (Go: `openAll`).
fn open_all(
    cfg: &Config,
    opts: gc::Options,
) -> Result<(Arc<packstore::Store>, Arc<refstore::Store>, gc::Collector), BenchError> {
    let store = Path::new(&cfg.store);
    let objects = Arc::new(packstore::Store::open_with(
        store.join("packstore"),
        packstore::Options::new()
            .sync(true)
            .segment_size(cfg.segment)
            .preallocate(cfg.preallocate),
    )?);
    let refs = match refstore::Store::open(store.join("refs"), true) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            let _ = objects.close();
            return Err(e.into());
        }
    };
    let coll = match gc::Collector::open(
        store.join("closures"),
        Arc::clone(&objects),
        Arc::clone(&refs),
        opts,
    ) {
        Ok(c) => c,
        Err(e) => {
            drop(refs);
            let _ = objects.close();
            return Err(e.into());
        }
    };
    Ok((objects, refs, coll))
}

/// Closes collector, refs, objects in that order, joining the errors (Go:
/// `closeAll`; the refs DB closes on its last Arc drop, with no error
/// surface).
fn close_all(
    objects: Arc<packstore::Store>,
    refs: Arc<refstore::Store>,
    coll: gc::Collector,
) -> Result<(), BenchError> {
    let mut errs = Vec::new();
    if let Err(e) = coll.close() {
        errs.push(e.to_string());
    }
    drop(coll); // releases the collector's Arcs on the stores
    drop(refs);
    if let Err(e) = objects.close() {
        errs.push(e.to_string());
    }
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs.join("\n").into())
    }
}

/// Mirrors the amber-store CLI's reference PUT: read old → prepare_ref
/// (walk + grey handoff) → refstore put → commit (abort on failure) →
/// release_ref(old) on overwrite (Go: `putRef`).
fn put_ref(
    coll: &gc::Collector,
    refs: &refstore::Store,
    name: &str,
    root: Key,
    raw: &[u8],
) -> Result<(), BenchError> {
    let mut old: Option<Key> = None;
    match refs.get(name) {
        Ok(prev) => {
            let prev_ref = Reference::decode(&prev)?;
            old = Some(Key::parse(&prev_ref.key)?);
        }
        Err(e) if e.is_not_found() => {}
        Err(e) => return Err(e.into()),
    }
    let prepared = coll.prepare_ref(root)?;
    match refs.put(name, raw) {
        Ok(()) => prepared.commit(),
        Err(e) => {
            prepared.abort();
            return Err(e.into());
        }
    }
    if let Some(old) = old {
        coll.release_ref(old)?;
    }
    Ok(())
}

/// Mirrors the amber-store CLI's `ref rm` (Go: `rmRef`).
fn rm_ref(coll: &gc::Collector, refs: &refstore::Store, name: &str) -> Result<(), BenchError> {
    let root = root_of(refs, name)?;
    refs.delete(name)?;
    coll.release_ref(root)?;
    Ok(())
}

/// Resolves a reference name to its root key (Go: `rootOf`).
fn root_of(refs: &refstore::Store, name: &str) -> Result<Key, BenchError> {
    let raw = refs.get(name)?;
    let r = Reference::decode(&raw)?;
    Ok(Key::parse(&r.key)?)
}

fn now_unix_nanos() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as i64)
}

// ---------------------------------------------------------------------------
// ingest (Go: phaseIngest).
// ---------------------------------------------------------------------------

fn phase_ingest(cfg: &Config, res: &mut Results) -> Result<(), BenchError> {
    if res.manifests.len() != cfg.refs {
        return Err(format!(
            "ingest: manifests for {} refs, want {} (run gen first)",
            res.manifests.len(),
            cfg.refs
        )
        .into());
    }
    let (objects, refs, coll) = open_all(cfg, gc::Options::default())?;
    let opts = ingest::Opts {
        jobs: available_parallelism(),
        chunk: ingest::ChunkOpts {
            // the CLI's defaults
            byte: Some(ByteOpts {
                min_size: 32 << 10,
                normal_size: 128 << 10,
                max_size: 256 << 10,
                ..Default::default()
            }),
            item_bits: ingest::DEFAULT_ITEM_BITS,
            xattr_inline_max: ingest::DEFAULT_XATTR_INLINE_MAX,
        },
        ..Default::default()
    };
    logf!("ingest: {} refs, jobs={}", cfg.refs, opts.jobs);
    res.ingest.clear();
    let start = Instant::now();
    let (mut logical, mut stored) = (0i64, 0i64);
    for i in 0..cfg.refs {
        let m = res.manifests[i].clone();
        let t0 = Instant::now();
        let (ws, root) = ingest::dir(&objects, ref_dir(cfg, i), opts.clone());
        let root = match root {
            Ok(k) => k,
            Err(e) => {
                let _ = close_all(objects, refs, coll);
                return Err(format!("ingest ref {i}: {e}").into());
            }
        };
        let t1 = Instant::now();
        let rec = Reference {
            name: ref_name(i),
            key: root.as_bytes().to_vec(),
            created_at: now_unix_nanos(),
            ..Default::default()
        };
        let put = rec
            .encode()
            .map_err(BenchError::from)
            .and_then(|raw| put_ref(&coll, &refs, &ref_name(i), root, &raw));
        if let Err(e) = put {
            let _ = close_all(objects, refs, coll);
            return Err(format!("ref {i}: {e}").into());
        }
        let t2 = Instant::now();
        res.ingest.push(RefIngest {
            index: i as i64,
            kept: m.kept,
            logical: m.logical,
            fresh: m.fresh,
            shared: m.shared,
            stored: ws.stored as i64,
            deduped: ws.deduped as i64,
            bytes_stored: ws.bytes_stored as i64,
            ingest_ns: ns(t1 - t0),
            ref_ns: ns(t2 - t1),
        });
        logical += m.logical;
        stored += ws.bytes_stored as i64;
        if (i + 1) % 50 == 0 || i + 1 == cfg.refs {
            let el = start.elapsed().as_secs_f64();
            logf!(
                "ingest: {:4}/{}  logical {}  stored {}  {:.0} MiB/s logical  {:.0} MiB/s stored",
                i + 1,
                cfg.refs,
                human(logical),
                human(stored),
                logical as f64 / MIB as f64 / el,
                stored as f64 / MIB as f64 / el
            );
        }
    }
    res.ingest_wall_ns = ns(start.elapsed());
    let tc = Instant::now();
    close_all(objects, refs, coll)?;
    res.ingest_close_ns = ns(tc.elapsed());
    logf!(
        "ingest: done in {} (+{} close)",
        fmt_ms(Duration::from_nanos(res.ingest_wall_ns as u64)),
        fmt_ms(Duration::from_nanos(res.ingest_close_ns as u64))
    );
    save_results(cfg, res)
}

// ---------------------------------------------------------------------------
// delete (Go: phaseDelete).
// ---------------------------------------------------------------------------

fn phase_delete(cfg: &Config, res: &mut Results) -> Result<(), BenchError> {
    let (objects, refs, coll) = open_all(cfg, gc::Options::default())?;
    logf!("delete: removing the deleted-class refs");
    let start = Instant::now();
    let mut n = 0i64;
    for i in 0..cfg.refs {
        if kept(i) {
            continue;
        }
        if let Err(e) = rm_ref(&coll, &refs, &ref_name(i)) {
            let _ = close_all(objects, refs, coll);
            return Err(format!("rm ref {i}: {e}").into());
        }
        n += 1;
    }
    res.delete_ns = ns(start.elapsed());
    res.delete_n = n;
    close_all(objects, refs, coll)?;
    logf!(
        "delete: {} refs in {}",
        n,
        fmt_ms(Duration::from_nanos(res.delete_ns as u64))
    );
    save_results(cfg, res)
}

// ---------------------------------------------------------------------------
// gc (Go: runCLI, phaseGC, snapshotStore, duKiB).
// ---------------------------------------------------------------------------

/// Runs the amber-store CLI, prepending `--store` and `--segment-size`, with
/// stdout and stderr captured into one combined buffer; `Args` records only
/// the caller's arguments (Go: `runCLI`, `exec.Cmd.CombinedOutput`).
fn run_cli(cfg: &Config, args: &[&str]) -> CliRun {
    let mut r = CliRun {
        args: args.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    };
    let t0 = Instant::now();
    match combined_output(cfg, args) {
        Ok((out, status)) => {
            r.output = String::from_utf8_lossy(&out).into_owned();
            if !status.success() {
                r.exit_err = status.to_string();
            }
        }
        Err(e) => r.exit_err = e.to_string(),
    }
    r.wall_ns = ns(t0.elapsed());
    r
}

/// Spawns the CLI with both stdout and stderr on one pipe, so the combined
/// output interleaves exactly as written (Go: `CombinedOutput`).
fn combined_output(cfg: &Config, args: &[&str]) -> io::Result<(Vec<u8>, ExitStatus)> {
    let (mut reader, writer) = io::pipe()?;
    let writer2 = writer.try_clone()?;
    let mut child = {
        // The block scope drops the Command — and with it the parent's pipe
        // write ends — so the read below sees EOF when the child exits.
        let mut c = Command::new(&cfg.bin);
        c.arg("--store")
            .arg(&cfg.store)
            .arg("--segment-size")
            .arg(cfg.segment.to_string())
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::from(writer))
            .stderr(Stdio::from(writer2));
        c.spawn()?
    };
    let mut buf = Vec::new();
    reader.read_to_end(&mut buf)?;
    let status = child.wait()?;
    Ok((buf, status))
}

fn phase_gc(cfg: &Config, res: &mut Results) -> Result<(), BenchError> {
    if cfg.bin.is_empty() {
        return Err("gc: --bin (or amber-store in PATH) is required".into());
    }
    thread::sleep(Duration::from_secs(2)); // every sealed pack crosses a 1 s grace
    for args in [
        vec!["gc", "run", "--grace", "1s"],
        vec!["gc", "run", "--grace", "1s", "--garbage", "0"],
    ] {
        let label = if args.contains(&"--garbage") {
            "after-gc-forced"
        } else {
            "after-gc-policy"
        };
        logf!("gc: {}", args.join(" "));
        let r = run_cli(cfg, &args);
        res.gc_runs.push(r.clone());
        logf!(
            "gc: {} wall: {}{}",
            fmt_ms(Duration::from_nanos(r.wall_ns as u64)),
            r.output.trim(),
            r.exit_err
        );
        if !r.exit_err.is_empty() {
            let _ = save_results(cfg, res);
            return Err(format!("gc: {}: {}", r.exit_err, r.output).into());
        }
        snapshot_store(cfg, res, label)?;
    }
    Ok(())
}

fn snapshot_store(cfg: &Config, res: &mut Results, label: &str) -> Result<(), BenchError> {
    let store = Path::new(&cfg.store);
    let mut s = Snapshot {
        label: label.to_string(),
        ..Default::default()
    };
    s.packstore_kib = du_kib(&store.join("packstore"))?;
    s.refs_kib = du_kib(&store.join("refs"))?;
    s.closures_kib = du_kib(&store.join("closures"))?;
    // Sealed + active segment files (Go globs *.seg and *.seg.active; glob
    // errors are ignored there, a failed read_dir counts zero here).
    if let Ok(rd) = fs::read_dir(store.join("packstore")) {
        for entry in rd.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.ends_with(".seg") || name.ends_with(".seg.active") {
                s.segments += 1;
            }
        }
    }
    s.free_bytes = free_bytes(&cfg.store);
    if !cfg.bin.is_empty() {
        let r = run_cli(cfg, &["gc", "status"]);
        if !r.exit_err.is_empty() {
            return Err(format!("gc status: {}: {}", r.exit_err, r.output).into());
        }
        // The full per-pack listing goes next to the results file; the
        // totals go into it.
        let listing = Path::new(&cfg.out)
            .parent()
            .unwrap_or(Path::new(""))
            .join(format!("gc-status-{label}.txt"));
        fs::write(listing, r.output.as_bytes())?;
        let totals: Vec<&str> = r
            .output
            .lines()
            .filter(|l| l.starts_with("live ") || l.starts_with("last cycle"))
            .collect();
        s.gc_status = totals.join("\n");
    }
    logf!(
        "snapshot {}: packstore {}, refs {}, closures {}, {} segments (incl. active); {}",
        label,
        human(s.packstore_kib << 10),
        human(s.refs_kib << 10),
        human(s.closures_kib << 10),
        s.segments,
        s.gc_status.replace('\n', " | ")
    );
    res.snapshots.push(s);
    save_results(cfg, res)
}

/// `du -sk dir`, first field (Go: `duKiB`; like Go's `fmt.Sscan`, an
/// unparsable first field leaves 0).
fn du_kib(dir: &Path) -> Result<i64, BenchError> {
    let out = Command::new("du")
        .arg("-sk")
        .arg(dir)
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("du {}: {e}", dir.display()))?;
    if !out.status.success() {
        return Err(format!("du {}: {}", dir.display(), out.status).into());
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .and_then(|t| t.parse().ok())
        .unwrap_or(0))
}

/// Free bytes on the filesystem holding `path`; errors leave 0 (Go:
/// `unix.Statfs` in `snapshotStore`). `statfs` rather than POSIX `statvfs`
/// for the same reason as gc's free-space probe: Darwin's `statvfs` truncates
/// block counts on large volumes.
fn free_bytes(path: &str) -> u64 {
    let Ok(cpath) = std::ffi::CString::new(path) else {
        return 0;
    };
    // SAFETY: cpath is a valid NUL-terminated C string and st a properly
    // aligned, zero-initialized statfs buffer; statfs only writes into it,
    // and it is read only after the call reports success.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(cpath.as_ptr(), &mut st) } != 0 {
        return 0;
    }
    (st.f_bavail as u64).saturating_mul(st.f_bsize as u64)
}

// ---------------------------------------------------------------------------
// verify (Go: phaseVerify).
// ---------------------------------------------------------------------------

fn phase_verify(cfg: &Config, res: &mut Results) -> Result<(), BenchError> {
    let (objects, refs, coll) = open_all(cfg, gc::Options::default())?;
    logf!("verify: CheckComplete on every kept ref");
    res.verify_complete = 0;
    res.verify_errors.clear();
    for i in 0..cfg.refs {
        if !kept(i) {
            match refs.get(&ref_name(i)) {
                Err(e) if e.is_not_found() => {}
                // Go formats the nil error of a still-present ref as <nil>.
                Ok(_) => res
                    .verify_errors
                    .push(format!("ref {i} still present: <nil>")),
                Err(e) => res
                    .verify_errors
                    .push(format!("ref {i} still present: {e}")),
            }
            continue;
        }
        let root = match root_of(&refs, &ref_name(i)) {
            Ok(k) => k,
            Err(e) => {
                res.verify_errors.push(format!("ref {i}: {e}"));
                continue;
            }
        };
        // The visited-keys list is discarded; only completeness matters.
        match fstree::check_complete(root, |k| objects.get(k), |k| objects.has(k), 0) {
            Ok(_) => res.verify_complete += 1,
            Err(e) => res.verify_errors.push(format!("ref {i} incomplete: {e}")),
        }
    }
    close_all(objects, refs, coll)?;
    logf!(
        "verify: {} refs complete, {} errors",
        res.verify_complete,
        res.verify_errors.len()
    );
    if !cfg.bin.is_empty() && !cfg.restore.is_empty() {
        // Byte-for-byte restore of a sample: kept refs whose predecessor was
        // deleted (i%10 == 0) share data with reaped packs — the risky case.
        res.verify_restore_ok.clear();
        let mut i = 10;
        while i < cfg.refs && i <= 100 {
            let dest = Path::new(&cfg.restore).join(format!("ref-{i:04}"));
            let _ = fs::remove_dir_all(&dest);
            let dest_s = dest.to_string_lossy().into_owned();
            let spec = format!("ref:{}", ref_name(i));
            let r = run_cli(cfg, &["restore", &spec, &dest_s]);
            if !r.exit_err.is_empty() {
                res.verify_errors
                    .push(format!("restore ref {i}: {}: {}", r.exit_err, r.output));
            } else {
                match Command::new("diff")
                    .arg("-rq")
                    .arg(ref_dir(cfg, i))
                    .arg(&dest)
                    .output()
                {
                    Ok(o) if o.status.success() => res.verify_restore_ok.push(ref_name(i)),
                    Ok(o) => {
                        let text = format!(
                            "{}{}",
                            String::from_utf8_lossy(&o.stdout),
                            String::from_utf8_lossy(&o.stderr)
                        );
                        res.verify_errors
                            .push(format!("diff ref {i}: {}: {text}", o.status));
                    }
                    Err(e) => res.verify_errors.push(format!("diff ref {i}: {e}: ")),
                }
            }
            let _ = fs::remove_dir_all(&dest);
            i += 10;
        }
        logf!(
            "verify: {} sample restores identical, {} errors total",
            res.verify_restore_ok.len(),
            res.verify_errors.len()
        );
    }
    save_results(cfg, res)
}

// ---------------------------------------------------------------------------
// report (Go: report, median, pct, human).
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)] // one Go function, ported linearly
fn report(w: &mut dyn Write, r: &Results) -> Result<(), BenchError> {
    if r.ingest.is_empty() || r.manifests.is_empty() {
        return Err("report: no ingest results yet".into());
    }
    let (mut logical, mut fresh, mut shared, mut stored) = (0i64, 0i64, 0i64, 0i64);
    let (mut n_stored, mut n_deduped, mut files) = (0i64, 0i64, 0usize);
    let (mut kept_fresh, mut kept_logical, mut del_fresh, mut del_logical) =
        (0i64, 0i64, 0i64, 0i64);
    for m in &r.manifests {
        files += m.files.len();
        if m.kept {
            kept_fresh += m.fresh;
            kept_logical += m.logical;
        } else {
            del_fresh += m.fresh;
            del_logical += m.logical;
        }
    }
    let (mut per_kept, mut per_del, mut ref_put) = (Vec::new(), Vec::new(), Vec::new());
    let (mut ingest_ns, mut ref_ns) = (0i64, 0i64);
    for x in &r.ingest {
        logical += x.logical;
        fresh += x.fresh;
        shared += x.shared;
        stored += x.bytes_stored;
        n_stored += x.stored;
        n_deduped += x.deduped;
        ingest_ns += x.ingest_ns;
        ref_ns += x.ref_ns;
        let ms = (x.ingest_ns + x.ref_ns) as f64 / 1e6;
        if x.kept {
            per_kept.push(ms);
        } else {
            per_del.push(ms);
        }
        ref_put.push(x.ref_ns as f64 / 1e6);
    }
    let (n_kept, n_del) = (per_kept.len(), per_del.len());
    let wall = r.ingest_wall_ns as f64 / 1e9;
    writeln!(
        w,
        "DATASET  refs={} files={} logical={} fresh={} shared(copies)={} overlap={:.1}%",
        r.manifests.len(),
        files,
        human(logical),
        human(fresh),
        human(shared),
        100.0 * shared as f64 / logical as f64
    )?;
    writeln!(
        w,
        "         kept {} refs: fresh {}, logical {}",
        n_kept,
        human(kept_fresh),
        human(kept_logical)
    )?;
    writeln!(
        w,
        "         deleted {} refs: fresh {}, logical {}",
        n_del,
        human(del_fresh),
        human(del_logical)
    )?;
    writeln!(w, "GEN      {:.1}s", r.gen_ns as f64 / 1e9)?;
    writeln!(
        w,
        "INGEST   wall {:.1}s  (ingest.Dir {:.1}s + ref put/closure walk {:.1}s; close {:.2}s)",
        wall,
        ingest_ns as f64 / 1e9,
        ref_ns as f64 / 1e9,
        r.ingest_close_ns as f64 / 1e9
    )?;
    writeln!(
        w,
        "         logical {:.0} MiB/s   new-bytes {:.0} MiB/s   refs {:.1}/s",
        logical as f64 / MIB as f64 / wall,
        stored as f64 / MIB as f64 / wall,
        r.ingest.len() as f64 / wall
    )?;
    writeln!(
        w,
        "         objects stored {}  deduped {}  bytes stored {}  ({:.1}% of ingested bytes deduplicated)",
        n_stored,
        n_deduped,
        human(stored),
        100.0 * (1.0 - stored as f64 / logical as f64)
    )?;
    if n_kept > 0 && n_del > 0 {
        writeln!(
            w,
            "         per-ref ms: kept median {:.0} p95 {:.0} max {:.0} | deleted-class median {:.0} p95 {:.0} max {:.0}",
            median(&per_kept),
            pct(&per_kept, 0.95),
            max_of(&per_kept),
            median(&per_del),
            pct(&per_del, 0.95),
            max_of(&per_del)
        )?;
    }
    let head = &ref_put[..100.min(ref_put.len())];
    let tail = &ref_put[ref_put.len().saturating_sub(100)..];
    writeln!(
        w,
        "         ref put (closure walk) ms: median {:.1} p95 {:.1} max {:.1}; first {} median {:.1}, last {} median {:.1}",
        median(&ref_put),
        pct(&ref_put, 0.95),
        max_of(&ref_put),
        head.len(),
        median(head),
        tail.len(),
        median(tail)
    )?;
    let mut window = 100;
    if r.ingest.len() < 200 {
        window = 1.max(r.ingest.len() / 5);
    }
    let mut start = 0;
    while start < r.ingest.len() {
        let xs = &r.ingest[start..(start + window).min(r.ingest.len())];
        let (mut lg, mut ti, mut tr) = (0i64, 0i64, 0i64);
        for x in xs {
            lg += x.logical;
            ti += x.ingest_ns;
            tr += x.ref_ns;
        }
        writeln!(
            w,
            "         refs {:4}-{:4}: {:6.0} MiB/s  ingest.Dir {:7.1} ms/ref  ref-put {:6.1} ms/ref",
            start,
            start + xs.len() - 1,
            lg as f64 / MIB as f64 / ((ti + tr) as f64 / 1e9),
            ti as f64 / 1e6 / xs.len() as f64,
            tr as f64 / 1e6 / xs.len() as f64
        )?;
        start += window;
    }
    if r.delete_n > 0 {
        writeln!(
            w,
            "DELETE   {} refs in {:.2}s  ({:.1} ms/ref)",
            r.delete_n,
            r.delete_ns as f64 / 1e9,
            r.delete_ns as f64 / 1e6 / r.delete_n as f64
        )?;
    }
    for run in &r.gc_runs {
        writeln!(
            w,
            "GC RUN   {}: wall {:.2}s -> {} {}",
            run.args.join(" "),
            run.wall_ns as f64 / 1e9,
            run.output.trim(),
            run.exit_err
        )?;
    }
    if !r.snapshots.is_empty() {
        writeln!(w, "SNAPSHOTS")?;
    }
    let mut snap: HashMap<&str, &Snapshot> = HashMap::new();
    for s in &r.snapshots {
        snap.insert(&s.label, s);
        writeln!(
            w,
            "  {:<16} packstore {:>10}  segments {:4}  closures {:>9}  refs-db {:>9}  | {}",
            s.label,
            human(s.packstore_kib << 10),
            s.segments,
            human(s.closures_kib << 10),
            human(s.refs_kib << 10),
            s.gc_status.replace('\n', " | ")
        )?;
    }
    if let (Some(a), Some(b)) = (snap.get("after-ingest"), snap.get("after-gc-policy")) {
        let (ab, bb) = (a.packstore_kib << 10, b.packstore_kib << 10);
        writeln!(
            w,
            "RECLAIM  nominal (fresh bytes owned by the deleted refs): {}",
            human(del_fresh)
        )?;
        writeln!(
            w,
            "         policy GC freed on disk: {} ({:.0}% of nominal); store {} -> {}",
            human(ab - bb),
            100.0 * (ab - bb) as f64 / del_fresh as f64,
            human(ab),
            human(bb)
        )?;
        if let Some(c) = snap.get("after-gc-forced") {
            let cb = c.packstore_kib << 10;
            writeln!(
                w,
                "         forced  GC freed on disk (cumulative): {} ({:.0}% of nominal); store -> {}",
                human(ab - cb),
                100.0 * (ab - cb) as f64 / del_fresh as f64,
                human(cb)
            )?;
        }
    }
    writeln!(
        w,
        "VERIFY   complete kept refs {}/{}; sample restores identical {}; errors [{}]",
        r.verify_complete,
        n_kept,
        r.verify_restore_ok.len(),
        r.verify_errors.join(" ")
    )?;
    Ok(())
}

fn median(xs: &[f64]) -> f64 {
    pct(xs, 0.5)
}

/// Sorts a clone and indexes at `len*q`, capped at the last element (Go:
/// `pct`).
fn pct(xs: &[f64], q: f64) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    let mut s = xs.to_vec();
    s.sort_by(f64::total_cmp);
    s[(s.len() - 1).min((s.len() as f64 * q) as usize)]
}

/// (Go: `slices.Max`; callers guarantee non-empty.)
fn max_of(xs: &[f64]) -> f64 {
    xs.iter().copied().fold(f64::NEG_INFINITY, f64::max)
}
