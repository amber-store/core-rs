//! Content-defined chunking: the two chunkers the fstree uses — a byte
//! chunker (UltraCDC) for file content, and an item chunker for the
//! index/entry streams (`architecture/fstree.md`, "Content-defined chunking").
//!
//! Port of the Go package `chunkers` plus the vendored
//! `github.com/PlakarKorp/go-cdc-chunkers` UltraCDC implementation and driver
//! loop (see the ISC notice below).

use std::io::Read;

/// Default minimum chunk size for the UltraCDC byte chunker (32 KiB).
///
/// The three defaults determine every Blob boundary and so every file key;
/// they match Go `chunkers.DefaultMinSize` / `DefaultNormalSize` /
/// `DefaultMaxSize`, and two stores dedup against each other only when they
/// chunk with the same sizes.
pub const DEFAULT_MIN_SIZE: usize = 32 * 1024;
/// Default normal (target) chunk size for the UltraCDC byte chunker (512 KiB).
pub const DEFAULT_NORMAL_SIZE: usize = 512 * 1024;
/// Default maximum chunk size for the UltraCDC byte chunker (1 MiB).
pub const DEFAULT_MAX_SIZE: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// UltraCDC byte chunker.
//
// The code below (options resolution, validation rules and messages, the
// cutpoint algorithm, and the driver semantics of `Chunker.Next`) is ported
// from github.com/PlakarKorp/go-cdc-chunkers v1.0.3, which carries this
// notice:
//
// Copyright (c) 2023 Gilles Chehade <gilles@poolp.org>
//
// Permission to use, copy, modify, and distribute this software for any
// purpose with or without fee is hereby granted, provided that the above
// copyright notice and this permission notice appear in all copies.
//
// THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
// WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
// MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
// ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
// WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
// ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
// OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
// ---------------------------------------------------------------------------

/// Configures the UltraCDC byte chunker (the Rust equivalent of the upstream
/// `ChunkerOpts`, re-exported by the Go package as `ByteOpts`).
///
/// A zero size field selects the UltraCDC default for that field
/// ([`DEFAULT_MIN_SIZE`] / [`DEFAULT_NORMAL_SIZE`] / [`DEFAULT_MAX_SIZE`]), so
/// `ByteOpts::default()` selects all defaults. `key` mirrors the upstream
/// `Key` field; UltraCDC ignores it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ByteOpts {
    pub min_size: usize,
    pub max_size: usize,
    pub normal_size: usize,
    pub key: Vec<u8>,
}

/// Invalid [`ByteOpts`], mirroring upstream's `ErrNormalSize` / `ErrMinSize` /
/// `ErrMaxSize` sentinels (same messages, same check order: normal, min, max).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum OptionsError {
    #[error("NormalSize is required and must be 64B <= NormalSize <= 1GB")]
    NormalSize,
    #[error("MinSize is required and must be 64B <= MinSize <= 1GB && MinSize < NormalSize")]
    MinSize,
    #[error("MaxSize is required and must be 64B <= MaxSize <= 1GB && MaxSize > NormalSize")]
    MaxSize,
}

/// Error from [`split_bytes`]: invalid options, a reader failure, or an error
/// returned by the chunk callback (each propagated exactly as in Go).
#[derive(Debug, thiserror::Error)]
pub enum SplitError<E> {
    #[error(transparent)]
    Options(#[from] OptionsError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Error returned by the chunk callback. Displays as the callback error
    /// alone (no added prefix), since Go's `SplitBytes` returns it unchanged.
    #[error("{0}")]
    Callback(E),
}

/// Effective (zero-resolved) options.
#[derive(Debug, Clone, Copy)]
struct Resolved {
    min_size: usize,
    max_size: usize,
    normal_size: usize,
}

impl Resolved {
    /// Zero fields select the defaults, per upstream `NewChunker`.
    fn new(opts: Option<&ByteOpts>) -> Resolved {
        let pick = |v: usize, d: usize| if v == 0 { d } else { v };
        match opts {
            None => Resolved {
                min_size: DEFAULT_MIN_SIZE,
                max_size: DEFAULT_MAX_SIZE,
                normal_size: DEFAULT_NORMAL_SIZE,
            },
            Some(o) => Resolved {
                min_size: pick(o.min_size, DEFAULT_MIN_SIZE),
                max_size: pick(o.max_size, DEFAULT_MAX_SIZE),
                normal_size: pick(o.normal_size, DEFAULT_NORMAL_SIZE),
            },
        }
    }

    /// Upstream `UltraCDC.Validate`, checks in the same order.
    fn validate(&self) -> Result<(), OptionsError> {
        const ONE_GIB: usize = 1024 * 1024 * 1024;
        if self.normal_size < 64 || self.normal_size > ONE_GIB {
            return Err(OptionsError::NormalSize);
        }
        if self.min_size < 64 || self.min_size > ONE_GIB || self.min_size >= self.normal_size {
            return Err(OptionsError::MinSize);
        }
        if self.max_size < 64 || self.max_size > ONE_GIB || self.max_size <= self.normal_size {
            return Err(OptionsError::MaxSize);
        }
        Ok(())
    }
}

/// `popcount(b ^ 0xAA)` — upstream's precomputed `hammingDistanceTo0xAA`.
const HAMMING_TO_0XAA: [i32; 256] = {
    let mut t = [0i32; 256];
    let mut b = 0usize;
    while b < 256 {
        t[b] = ((b as u8) ^ 0xAA).count_ones() as i32;
        b += 1;
    }
    t
};

/// Upstream `UltraCDC.Algorithm`, with `n = data.len()` (the driver always
/// passes the full window, so the Go `n` parameter is folded in).
///
/// Returns the cutpoint (exclusive index); a typical use is
/// `chunk = &data[..cutpoint]`. POST invariant: `cutpoint <= data.len()`.
fn ultra_cdc_cutpoint(opts: &Resolved, data: &[u8]) -> usize {
    const MASK_S: u64 = 0x2F; // binary 101111
    // MASK_L ignores 2 more bits than MASK_S, so it is easier to match (a
    // higher probability of a match after the normal point).
    const MASK_L: u64 = 0x2C; // binary 101100
    const LOW_ENTROPY_STRING_THRESHOLD: usize = 64; // LEST in the paper.

    let min_size = opts.min_size;
    let max_size = opts.max_size;
    let mut normal_size = opts.normal_size;

    let mut low_entropy_count = 0usize;

    // Initial mask for small cuts below the normal point.
    let mut mask = MASK_S;

    let mut n = data.len();
    if n <= min_size {
        return n;
    } else if n >= max_size {
        n = max_size;
    } else if n <= normal_size {
        normal_size = n;
    }

    // Go slices data[minSize : minSize+8] here even when n < minSize+8,
    // relying on spare capacity behind the peeked bufio slice. On a genuine
    // short tail the spare capacity is always there (bufio slides the data to
    // the buffer start before a short fill), and the out-of-len bytes read are
    // never used: the loop below runs only when minSize+8 <= n-8. But for
    // configs with maxSize < minSize+8 (valid per Validate) the mid-stream
    // window sits at the end of the bufio buffer and Go PANICS with a slice
    // out-of-range from the second chunk on (verified against Go directly).
    // Guard instead: identical cutpoints wherever Go does not crash.
    if n < min_size + 8 {
        return n;
    }

    let mut out_buf_win: &[u8] = &data[min_size..min_size + 8];

    // Initialize the hamming distance to the 0xAA…AA pattern on out_buf_win,
    // one byte at a time.
    let mut dist: i64 = out_buf_win
        .iter()
        .map(|&v| i64::from(HAMMING_TO_0XAA[v as usize]))
        .sum();

    for i in (min_size + 8..=n - 8).step_by(8) {
        if i >= normal_size {
            // Upstream writes the mask every iteration after the normal
            // point on purpose; keep the same structure.
            mask = MASK_L;
        }

        // If i == n-8 then i+8 == n, so we never go out of bounds.
        let in_buf_win = &data[i..i + 8];

        if in_buf_win == out_buf_win {
            low_entropy_count += 1;
            if low_entropy_count >= LOW_ENTROPY_STRING_THRESHOLD {
                // If i == n-8, its largest, this returns n, which maintains
                // the POST invariant that cutpoint <= n.
                return i + 8;
            }
            // Note: out_buf_win and dist are deliberately NOT updated here,
            // exactly as upstream.
            continue;
        }

        low_entropy_count = 0;
        for j in 0..8 {
            if (dist as u64) & mask == 0 {
                // Largest possible return here is (n-8) + 7 == n-1.
                return i + j;
            }
            let out_byte = data[i + j - 8];
            let in_byte = data[i + j];
            dist += i64::from(HAMMING_TO_0XAA[in_byte as usize])
                - i64::from(HAMMING_TO_0XAA[out_byte as usize]);
        }
        out_buf_win = in_buf_win;
    }

    n
}

/// Runs the UltraCDC content-defined chunker over `reader` and calls `f` once
/// per chunk, in order. Each chunk passed to `f` is owned, so `f` may retain
/// it. `None` options (or zero fields) use the default sizes (min 32 KiB,
/// normal 512 KiB, max 1 MiB). An empty reader yields zero chunks.
///
/// This is the Go `chunkers.SplitBytes` wrapper fused with the upstream
/// `Chunker.Next` driver: up to `max_size` bytes are buffered from the reader
/// and presented to the algorithm, the chunk `[0..cutpoint]` is emitted, the
/// buffer advances, and the loop repeats until the input is exhausted. A
/// reader error discards any buffered bytes (as upstream does); a callback
/// error stops the split immediately.
pub fn split_bytes<R, E, F>(
    mut reader: R,
    opts: Option<&ByteOpts>,
    mut f: F,
) -> Result<(), SplitError<E>>
where
    R: Read,
    F: FnMut(Vec<u8>) -> Result<(), E>,
{
    let resolved = Resolved::new(opts);
    resolved.validate()?;
    let max_size = resolved.max_size;

    let mut buf: Vec<u8> = Vec::with_capacity(max_size);
    let mut eof = false;
    loop {
        // Refill to max_size bytes, or to EOF (upstream: bufio Peek(MaxSize)).
        if !eof && buf.len() < max_size {
            let want = max_size - buf.len();
            let got = reader
                .by_ref()
                .take(want as u64)
                .read_to_end(&mut buf)
                .map_err(SplitError::Io)?;
            if got < want {
                eof = true;
            }
        }
        if buf.is_empty() {
            // Empty input, or the previous chunk consumed the final bytes.
            return Ok(());
        }
        let cutpoint = ultra_cdc_cutpoint(&resolved, &buf);
        // cutpoint >= 1 whenever the buffer is non-empty, so like Go's
        // SplitBytes we never hand an empty chunk to the callback.
        f(buf[..cutpoint].to_vec()).map_err(SplitError::Callback)?;
        buf.drain(..cutpoint);
    }
}

// ---------------------------------------------------------------------------
// Item chunker.
// ---------------------------------------------------------------------------

/// Decides chunk boundaries between whole items (a child key, a directory
/// entry, or a `[sepName, childKey]` pair). A boundary ends the current run
/// when the low `k` bits of BLAKE3(item encoding) are zero (average run
/// `2^k`), bounded by `min_run` and `max_run` so an item is never split and
/// variance is capped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemChunker {
    pub min_run: usize,
    pub max_run: usize,
    mask: u64,
}

impl ItemChunker {
    /// Returns an item chunker with average run `2^bits` and derived bounds
    /// `min_run = 2^bits / 4` (at least 2) and `max_run = 2^bits * 4`.
    pub fn new(bits: u32) -> ItemChunker {
        let avg = 1usize << bits;
        let min_run = (avg / 4).max(2);
        let mask = if bits > 0 { (1u64 << bits) - 1 } else { 0 };
        ItemChunker {
            min_run,
            max_run: avg * 4,
            mask,
        }
    }

    /// Reports whether the run should end after the item whose canonical
    /// encoding is `enc`, given the current run length (including this item).
    pub fn is_boundary(&self, enc: &[u8], run_len: usize) -> bool {
        if run_len >= self.max_run {
            return true;
        }
        if run_len < self.min_run {
            return false;
        }
        let sum = blake3::hash(enc);
        let mut first8 = [0u8; 8];
        first8.copy_from_slice(&sum.as_bytes()[..8]);
        u64::from_le_bytes(first8) & self.mask == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// splitmix64 (VECTORS.md) for deterministic pseudo-random test data.
    fn splitmix_data(seed: u64, n: usize) -> Vec<u8> {
        let mut state = seed;
        let mut out = Vec::with_capacity(n + 8);
        while out.len() < n {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            out.extend_from_slice(&(z ^ (z >> 31)).to_le_bytes());
        }
        out.truncate(n);
        out
    }

    fn collect(input: &[u8], opts: Option<&ByteOpts>) -> Vec<Vec<u8>> {
        let mut chunks = Vec::new();
        split_bytes(input, opts, |c| {
            chunks.push(c);
            Ok::<(), std::convert::Infallible>(())
        })
        .expect("split_bytes");
        chunks
    }

    #[test]
    fn split_bytes_reassembles_input() {
        let input = splitmix_data(1, 5 * 1024 * 1024);
        let chunks = collect(&input, None);
        assert!(
            chunks.len() >= 2,
            "expected multiple chunks for 5 MiB, got {}",
            chunks.len()
        );
        let got: Vec<u8> = chunks.concat();
        assert_eq!(got, input, "reassembled bytes differ from input");
        for c in &chunks[..chunks.len() - 1] {
            assert!(c.len() >= DEFAULT_MIN_SIZE && c.len() <= DEFAULT_MAX_SIZE);
        }
    }

    #[test]
    fn split_bytes_empty_input_yields_no_chunks() {
        assert_eq!(collect(&[], None).len(), 0);
    }

    #[test]
    fn split_bytes_chunks_are_owned_copies_safe_to_retain() {
        let input: Vec<u8> = (0..2 * 1024 * 1024).map(|i| i as u8).collect();
        let retained = collect(&input, None); // retained without copying
        let got: Vec<u8> = retained.concat();
        assert_eq!(got, input, "retained chunks must stay intact");
    }

    #[test]
    fn split_bytes_short_inputs_are_single_chunks() {
        // n <= min_size, and min_size < n < min_size+16 (loop cannot run):
        // the whole input is one chunk.
        for n in [
            1,
            64,
            DEFAULT_MIN_SIZE,
            DEFAULT_MIN_SIZE + 1,
            DEFAULT_MIN_SIZE + 15,
        ] {
            let input = splitmix_data(2, n);
            let chunks = collect(&input, None);
            assert_eq!(chunks.len(), 1, "input len {n}");
            assert_eq!(chunks[0], input, "input len {n}");
        }
    }

    #[test]
    fn split_bytes_low_entropy_cut() {
        // Constant data: every window equals the previous one, so the LEST
        // path fires after 64 equal windows: cut at minSize+8 + 63*8 + 8 =
        // minSize + 520.
        let input = vec![0xAAu8; DEFAULT_MIN_SIZE + 10_000];
        let chunks = collect(&input, None);
        assert_eq!(chunks[0].len(), DEFAULT_MIN_SIZE + 520);
        let got: Vec<u8> = chunks.concat();
        assert_eq!(got, input);
    }

    #[test]
    fn split_bytes_respects_custom_sizes() {
        let opts = ByteOpts {
            min_size: 64,
            max_size: 256,
            normal_size: 128,
            ..ByteOpts::default()
        };
        let input = splitmix_data(3, 10_000);
        let mut total = 0usize;
        let chunks = collect(&input, Some(&opts));
        for (i, c) in chunks.iter().enumerate() {
            assert!(c.len() <= 256, "chunk {i} over max");
            if i != chunks.len() - 1 {
                assert!(c.len() >= 64, "chunk {i} under min");
            }
            total += c.len();
        }
        assert_eq!(total, input.len());
    }

    #[test]
    fn split_bytes_zero_fields_select_defaults() {
        // Only min_size set: normal/max resolve to defaults and validate.
        let opts = ByteOpts {
            min_size: 128,
            ..ByteOpts::default()
        };
        let input = splitmix_data(4, 100_000);
        let got: Vec<u8> = collect(&input, Some(&opts)).concat();
        assert_eq!(got, input);
        // All-zero opts behave exactly like None.
        let a = collect(&input, Some(&ByteOpts::default()));
        let b = collect(&input, None);
        assert_eq!(a, b);
    }

    #[test]
    fn split_bytes_validates_options_in_upstream_order() {
        let run = |min, normal, max| -> Result<(), SplitError<std::convert::Infallible>> {
            let opts = ByteOpts {
                min_size: min,
                max_size: max,
                normal_size: normal,
                ..ByteOpts::default()
            };
            split_bytes(&b"x"[..], Some(&opts), |_| Ok(()))
        };
        let opts_err = |r: Result<(), SplitError<std::convert::Infallible>>| match r {
            Err(SplitError::Options(e)) => e,
            other => panic!("expected options error, got {other:?}"),
        };
        assert_eq!(opts_err(run(0, 32, 0)), OptionsError::NormalSize);
        assert_eq!(
            opts_err(run(0, 2 * 1024 * 1024 * 1024, 0)),
            OptionsError::NormalSize
        );
        assert_eq!(opts_err(run(32, 128, 256)), OptionsError::MinSize);
        assert_eq!(opts_err(run(128, 128, 256)), OptionsError::MinSize);
        assert_eq!(
            opts_err(run(1024 * 1024 * 1024 + 1, 8192, 2 * 1024 * 1024)),
            OptionsError::MinSize
        );
        assert_eq!(opts_err(run(64, 128, 128)), OptionsError::MaxSize);
        assert_eq!(opts_err(run(64, 128, 96)), OptionsError::MaxSize);
        assert_eq!(opts_err(run(64, 128, 63)), OptionsError::MaxSize);
        assert_eq!(
            opts_err(run(64, 128, 1024 * 1024 * 1024 + 1)),
            OptionsError::MaxSize
        );
        // Order: an invalid normal wins over an invalid min.
        assert_eq!(opts_err(run(16, 32, 0)), OptionsError::NormalSize);
        // Zero fields resolve to defaults BEFORE validation (Go: NewChunker
        // fills defaults, then the Validate rules apply): normal=0 becomes
        // 10240, so max=96 fails as MaxSize, not NormalSize.
        assert_eq!(opts_err(run(64, 0, 96)), OptionsError::MaxSize);
        // Tightest valid config: 64 <= min < normal < max.
        assert!(run(64, 65, 66).is_ok());
        assert_eq!(
            OptionsError::NormalSize.to_string(),
            "NormalSize is required and must be 64B <= NormalSize <= 1GB"
        );
        assert_eq!(
            OptionsError::MinSize.to_string(),
            "MinSize is required and must be 64B <= MinSize <= 1GB && MinSize < NormalSize"
        );
        assert_eq!(
            OptionsError::MaxSize.to_string(),
            "MaxSize is required and must be 64B <= MaxSize <= 1GB && MaxSize > NormalSize"
        );
    }

    /// The mask actually switches at the normal point: a window with hamming
    /// distance 3 to the 0xAA pattern satisfies `3 & maskL == 0` but
    /// `3 & maskS != 0`. Data: all 0xAA except data[71] = 0xAD, so the window
    /// data[64..72] has dist 3 and every later window (all 0xAA) differs from
    /// it. Expected chunk lengths verified against the Go implementation.
    #[test]
    fn split_bytes_mask_switches_at_normal_point() {
        let mut data = vec![0xAAu8; 1000];
        data[71] = 0xAD;

        // normal=2048 keeps maskS active: 3 & 0x2F != 0, no early cut; the
        // low-entropy path closes the first chunk instead.
        let opts_s = ByteOpts {
            min_size: 64,
            max_size: 4096,
            normal_size: 2048,
            ..ByteOpts::default()
        };
        assert_eq!(
            collect(&data, Some(&opts_s))
                .iter()
                .map(Vec::len)
                .collect::<Vec<_>>(),
            [592, 408],
            "maskS: no cut from dist 3"
        );

        // normal=72 makes the very first iteration use maskL: 3 & 0x2C == 0,
        // immediate cut at i = minSize+8 = 72.
        let opts_l = ByteOpts {
            min_size: 64,
            max_size: 4096,
            normal_size: 72,
            ..ByteOpts::default()
        };
        assert_eq!(
            collect(&data, Some(&opts_l))
                .iter()
                .map(Vec::len)
                .collect::<Vec<_>>(),
            [72, 584, 344],
            "maskL: immediate cut from dist 3"
        );
    }

    /// dist == 0 at the first window cuts immediately at minSize+8, checked
    /// before any dist update (j == 0). Expected lengths verified against Go.
    #[test]
    fn split_bytes_dist_zero_cuts_before_update() {
        let mut data = vec![0u8; 1000];
        data[64..72].fill(0xAA); // out_buf_win dist == 0
        let opts = ByteOpts {
            min_size: 64,
            max_size: 4096,
            normal_size: 2048,
            ..ByteOpts::default()
        };
        assert_eq!(
            collect(&data, Some(&opts))
                .iter()
                .map(Vec::len)
                .collect::<Vec<_>>(),
            [72, 584, 344]
        );
    }

    /// Constant 0x55 data has dist 64 per window (64 & maskS == 0), but the
    /// equality branch runs first, so the LEST path decides the cut — the
    /// dist check is never reached for identical windows. Verified against Go.
    #[test]
    fn split_bytes_equal_windows_take_lest_path_even_when_dist_matches() {
        let data = vec![0x55u8; 30000];
        let opts = ByteOpts {
            min_size: 64,
            max_size: 4096,
            normal_size: 512,
            ..ByteOpts::default()
        };
        let lens: Vec<usize> = collect(&data, Some(&opts)).iter().map(Vec::len).collect();
        // 64 + 8*65 = 584 per full chunk, 30000 = 51*584 + 216.
        assert_eq!(lens[..51], [584; 51]);
        assert_eq!(lens[51], 216);
    }

    /// max_size < min_size+8 (valid per upstream Validate): every full window
    /// is clamped to max and returned whole. Go PANICS on this config from
    /// the second chunk on (slice out of range past the peeked window's
    /// capacity); Rust guards and keeps chunking — see port-notes.
    #[test]
    fn split_bytes_pathological_max_below_min_plus_8() {
        let opts = ByteOpts {
            min_size: 64,
            max_size: 66,
            normal_size: 65,
            ..ByteOpts::default()
        };
        let data = splitmix_data(7, 1000);
        let lens: Vec<usize> = collect(&data, Some(&opts)).iter().map(Vec::len).collect();
        assert_eq!(lens[..15], [66; 15]);
        assert_eq!(lens[15], 10); // 1000 = 15*66 + 10
    }

    #[test]
    fn split_bytes_reader_error_discards_buffered_bytes() {
        struct FailAfter {
            data: &'static [u8],
        }
        impl Read for FailAfter {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                if self.data.is_empty() {
                    return Err(std::io::Error::other("boom"));
                }
                let k = self.data.len().min(out.len());
                out[..k].copy_from_slice(&self.data[..k]);
                self.data = &self.data[k..];
                Ok(k)
            }
        }
        let mut calls = 0;
        let r = FailAfter { data: &[0x55; 100] };
        let err = split_bytes(r, None, |_| {
            calls += 1;
            Ok::<(), std::convert::Infallible>(())
        })
        .expect_err("reader error must propagate");
        assert!(matches!(err, SplitError::Io(_)));
        // Like Go's Next, buffered bytes are not emitted on a read error.
        assert_eq!(calls, 0);
    }

    #[test]
    fn split_bytes_callback_error_stops_iteration() {
        let input = splitmix_data(5, 1024 * 1024);
        let mut calls = 0;
        let err = split_bytes(&input[..], None, |_| {
            calls += 1;
            Err("stop")
        })
        .expect_err("callback error must propagate");
        // Displays as the callback error alone, as Go returns it unchanged.
        assert_eq!(err.to_string(), "stop");
        match err {
            SplitError::Callback(e) => assert_eq!(e, "stop"),
            other => panic!("expected callback error, got {other:?}"),
        }
        assert_eq!(calls, 1);
    }

    #[test]
    fn hamming_table_matches_upstream() {
        // Spot-check against the upstream precomputed table.
        let head = [4, 5, 3, 4, 5, 6, 4, 5, 3, 4, 2, 3, 4, 5, 3, 4];
        assert_eq!(&HAMMING_TO_0XAA[..16], &head);
        assert_eq!(HAMMING_TO_0XAA[0xAA], 0);
        assert_eq!(HAMMING_TO_0XAA[0x55], 8);
    }

    #[test]
    fn new_item_chunker_derives_bounds() {
        let c = ItemChunker::new(7); // avg 128
        assert_eq!((c.min_run, c.max_run), (32, 512));
        let c0 = ItemChunker::new(0); // avg 1: min clamps to 2
        assert_eq!((c0.min_run, c0.max_run), (2, 4));
    }

    #[test]
    fn is_boundary_below_min_never_boundary() {
        let c = ItemChunker::new(7);
        for run_len in 1..c.min_run {
            assert!(
                !c.is_boundary(b"anything", run_len),
                "boundary at run_len {run_len} below min_run {}",
                c.min_run
            );
        }
    }

    #[test]
    fn is_boundary_at_max_always_boundary() {
        let c = ItemChunker::new(7);
        assert!(
            c.is_boundary(b"x", c.max_run),
            "no forced boundary at max_run"
        );
    }

    #[test]
    fn is_boundary_deterministic() {
        let c = ItemChunker::new(5);
        let enc = b"some item encoding";
        let first = c.is_boundary(enc, 100);
        for _ in 0..5 {
            assert_eq!(
                c.is_boundary(enc, 100),
                first,
                "is_boundary not deterministic"
            );
        }
    }

    #[test]
    fn is_boundary_hits_on_low_bits_zero() {
        // With k=0 the mask is 0, so every item at or above min_run is a
        // boundary.
        let c = ItemChunker::new(0);
        assert!(c.is_boundary(b"x", c.min_run));
        assert!(c.is_boundary(b"anything else", c.min_run + 5));
    }

    #[test]
    fn is_boundary_uses_little_endian_low_bits() {
        // Find an encoding whose BLAKE3 digest has its low 4 bits (of the LE
        // u64 of the first 8 bytes, i.e. of byte 0) zero, and one that does
        // not, then check is_boundary agrees.
        let c = ItemChunker::new(4);
        let mut saw = [false, false];
        for i in 0u32..1024 {
            let enc = i.to_le_bytes();
            let digest = blake3::hash(&enc);
            let want = digest.as_bytes()[0] & 0x0F == 0;
            assert_eq!(c.is_boundary(&enc, c.min_run), want, "item {i}");
            saw[usize::from(want)] = true;
        }
        assert_eq!(saw, [true, true], "test data should hit both outcomes");
    }
}
