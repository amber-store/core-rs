//! Bulk absence check (Go: `packstore/missing.go`).

use std::thread;

use crate::key::Key;

use super::{Error, Store};

/// Caps how many workers a small key list spawns: a chunk never holds fewer
/// keys than this, so the thread overhead stays amortized (Go:
/// `minMissingChunk`).
const MIN_MISSING_CHUNK: usize = 64;

/// The concurrency bound (Go: `maxParallel`, an errgroup limit; here it caps
/// the worker count directly — the chunk concatenation is order-preserving
/// either way).
const MAX_PARALLEL: usize = 16;

/// Computes worker count and chunk length exactly as Go does, clamped to
/// [`MAX_PARALLEL`] live workers.
fn plan(len: usize, parallelism: usize) -> (usize, usize) {
    let mut workers = parallelism;
    let m = len.div_ceil(MIN_MISSING_CHUNK);
    if m < workers {
        workers = m;
    }
    workers = workers.min(MAX_PARALLEL);
    if workers == 0 {
        return (0, 0);
    }
    (workers, len.div_ceil(workers))
}

/// The half-open key range worker `i` scans. Both bounds clamp: with many
/// workers the rounded-up chunk length can push a late worker's window past
/// the end (Go: the `min` clamps in `Missing`).
fn chunk_bounds(len: usize, chunk_len: usize, i: usize) -> (usize, usize) {
    let lo = (i * chunk_len).min(len);
    let hi = ((i + 1) * chunk_len).min(len);
    (lo, hi)
}

impl Store {
    /// Reports which of `keys` are absent from the store, preserving the
    /// input's order and multiplicity. Lookups run concurrently over
    /// contiguous chunks of the input (Go: `Missing`).
    pub fn missing(&self, keys: &[Key]) -> Result<Vec<Key>, Error> {
        let parallelism = thread::available_parallelism().map_or(1, |n| n.get());
        let (workers, chunk_len) = plan(keys.len(), parallelism);
        if workers == 0 {
            return Ok(Vec::new());
        }
        // Misses are what this call expects, so it looks at the directory
        // once, up front, rather than after each.
        self.refresh_after_miss(false)?;
        thread::scope(|s| {
            let handles: Vec<_> = (0..workers)
                .map(|i| {
                    let (lo, hi) = chunk_bounds(keys.len(), chunk_len, i);
                    let chunk = &keys[lo..hi];
                    s.spawn(move || -> Result<Vec<Key>, Error> {
                        let mut miss = Vec::new();
                        for &k in chunk {
                            let has = self.has_local(k).map_err(|e| Error::Context {
                                msg: format!("missing-check {k}"),
                                source: Box::new(e),
                            })?;
                            if !has {
                                miss.push(k);
                            }
                        }
                        Ok(miss)
                    })
                })
                .collect();
            let mut missing = Vec::new();
            let mut first_err: Option<Error> = None;
            for h in handles {
                match h.join() {
                    Ok(Ok(mut chunk_miss)) => missing.append(&mut chunk_miss),
                    Ok(Err(e)) => {
                        if first_err.is_none() {
                            first_err = Some(e);
                        }
                    }
                    Err(panic) => std::panic::resume_unwind(panic),
                }
            }
            match first_err {
                Some(e) => Err(e),
                None => Ok(missing),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Go regression: GOMAXPROCS=67 with 4289 keys made worker 66 compute
    // keys[4290:4289] and panic before the low bound was clamped. The plan +
    // clamp arithmetic must keep every window in bounds and cover the input
    // exactly once.
    #[test]
    fn chunk_bounds_cover_input_exactly() {
        for (len, par) in [
            (4289usize, 67usize),
            (0, 8),
            (1, 8),
            (64, 1),
            (65, 16),
            (1000, 3),
        ] {
            let (workers, chunk_len) = plan(len, par);
            if len == 0 {
                assert_eq!(workers, 0, "len=0 must plan zero workers");
                continue;
            }
            assert!((1..=MAX_PARALLEL).contains(&workers));
            let mut covered = 0usize;
            for i in 0..workers {
                let (lo, hi) = chunk_bounds(len, chunk_len, i);
                assert!(
                    lo <= hi && hi <= len,
                    "window [{lo},{hi}) out of bounds for len {len}"
                );
                assert_eq!(lo, covered, "gap before worker {i}");
                covered = hi;
            }
            assert_eq!(covered, len, "workers must cover the whole input");
        }
    }

    #[test]
    fn plan_respects_min_chunk() {
        // 100 keys → at most ceil(100/64)=2 workers no matter the parallelism.
        let (workers, chunk_len) = plan(100, 32);
        assert_eq!(workers, 2);
        assert_eq!(chunk_len, 50);
    }
}
