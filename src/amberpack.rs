//! The Amber-Store pack format. The content-addressed record codec is shared
//! by packstore's on-disk segments and the remote-sync wire packs defined here.
//!
//! A wire pack is a possibly-partial, unordered set of CAS objects (like a git
//! pack) carrying no root key. Layout:
//!
//! ```text
//! Magic    "AMBERPK\x03"   8 bytes  (plaintext)
//! Records  repeat: one encode_record output each — a 46-byte header
//!          (tag 0x01 + key[32] + flags + ulen + slen + CRC) followed by the payload
//! End      0x00
//! ```
//!
//! Each record is the same self-describing, CRC-protected, per-record-zstd unit
//! packstore writes on disk; a wire pack is just those records framed by a
//! magic and an explicit end marker, so a truncated stream is detected rather
//! than read as a clean EOF. The [`Reader`] validates framing, CRC, and key
//! canonicality and decodes each payload (its `Iterator` impl, Go's `All`), or
//! hands the validated records over undecoded ([`Reader::records`]); it does
//! NOT verify the payload hash — that happens in the storage path (packstore's
//! parallel write with Verify).
//!
//! Versions 1 and 2 (`AMBERPK\x01` / `AMBERPK\x02`) were the older uncompressed
//! and whole-stream-zstd stream formats; they are no longer produced and are
//! rejected by the [`Reader`].
//!
//! Compatibility note: record *headers* and raw (uncompressed) records are
//! byte-identical with the Go implementation. zstd-compressed payload frames
//! are not byte-identical (Go uses `klauspost/compress`, this port uses
//! libzstd), but each side decodes the other's frames; see PORTING.md.

use std::io::{self, BufReader, BufWriter, Read, Write};

use crate::key::Key;

/// The fixed record-header length:
/// tag(1) + key(32) + flags(1) + ulen(4) + slen(4) + crc(4). Payload follows.
pub const REC_HEADER_SIZE: usize = 46;

const TAG_CHUNK: u8 = 0x01;
const FLAG_ZSTD: u8 = 0x01;

/// Bounds one object's payload, stored or decoded. The length fields are
/// untrusted and size allocations. Real objects are ~1 MiB.
pub const MAX_PAYLOAD: u32 = 256 << 20;

/// Identifies the wire pack format and its version (the trailing byte).
const PACK_MAGIC: &[u8; 8] = b"AMBERPK\x03";

/// Marks the end of the record stream. A record begins with `TAG_CHUNK`
/// (0x01, written by [`encode_record`]), so the two are distinguished on the
/// first byte.
const TAG_END: u8 = 0x00;

/// Errors from the amberpack codec, mirroring the Go package's two `errors.Is`
/// sentinels (`ErrCorrupt`, `ErrMalformed`) plus the encode-side size error and
/// I/O passthrough. Match the class with [`Error::is_corrupt`] /
/// [`Error::is_malformed`] where Go code would use `errors.Is`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Record-level corruption surfaced by [`parse_record`] and
    /// [`decode_payload`] (bad framing, bad flags, CRC mismatch, length
    /// inconsistency, non-canonical key). It is the record-level counterpart
    /// to the stream-level `Malformed`. The packstore module reuses this class
    /// for its footer- and scrub-level corruption too, so the message stays
    /// deliberately general rather than naming "record".
    #[error("amberpack: corrupt pack data: {0}")]
    Corrupt(String),
    /// A structurally invalid wire pack (bad or legacy magic, truncation, an
    /// oversized or bad record, or a corrupt record). Note that, exactly like
    /// Go's `%w: %v` wrapping, a corrupt record inside a stream surfaces as
    /// `Malformed` (whose message embeds the corrupt-record text), not as
    /// `Corrupt`.
    #[error("amberpack: malformed pack stream: {0}")]
    Malformed(String),
    /// The payload handed to [`encode_record`] exceeds [`MAX_PAYLOAD`].
    #[error("amberpack: object {key} too large: {len} bytes")]
    TooLarge {
        /// The key the oversized payload was to be stored under.
        key: Key,
        /// The payload length in bytes.
        len: usize,
    },
    /// An I/O error from the underlying writer. (The [`Reader`] never returns
    /// this: exactly like Go, every stream read failure is classified
    /// `Malformed`.)
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl Error {
    /// Go's `errors.Is(err, ErrCorrupt)`.
    pub fn is_corrupt(&self) -> bool {
        matches!(self, Error::Corrupt(_))
    }

    /// Go's `errors.Is(err, ErrMalformed)`.
    pub fn is_malformed(&self) -> bool {
        matches!(self, Error::Malformed(_))
    }
}

/// A parsed record header. The payload lives at
/// `[REC_HEADER_SIZE .. REC_HEADER_SIZE + slen]` within the record's bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    /// The object's 32-byte lookup key.
    pub key: Key,
    /// The raw record flag byte; pass it to [`decode_payload`] unchanged.
    pub flags: u8,
    /// Uncompressed payload length.
    pub ulen: u32,
    /// Stored payload length (on the wire / on disk).
    pub slen: u32,
}

/// Reports whether a payload of `n` bytes is within [`MAX_PAYLOAD`]. Split out
/// of [`encode_record`] so the bound is testable without allocating it.
fn payload_fits(n: usize) -> bool {
    n as u64 <= u64::from(MAX_PAYLOAD)
}

/// Serializes `(k, data)` into a complete record, compressing the payload with
/// zstd when that makes it strictly smaller. `k` is written as given;
/// canonical-form validation happens on the read side.
pub fn encode_record(k: Key, data: &[u8]) -> Result<Vec<u8>, Error> {
    if !payload_fits(data.len()) {
        return Err(Error::TooLarge {
            key: k,
            len: data.len(),
        });
    }
    // Level 3 is the libzstd default, matching Go's klauspost default level.
    // A compression failure (allocation, in practice impossible) falls back to
    // raw storage — indistinguishable from "did not get smaller".
    let comp = zstd::bulk::compress(data, zstd::DEFAULT_COMPRESSION_LEVEL).ok();
    let (payload, flags): (&[u8], u8) = match comp.as_deref() {
        Some(c) if c.len() < data.len() => (c, FLAG_ZSTD),
        _ => (data, 0),
    };
    let mut rec = vec![0u8; REC_HEADER_SIZE + payload.len()];
    rec[0] = TAG_CHUNK;
    rec[1..33].copy_from_slice(k.as_bytes());
    rec[33] = flags;
    rec[34..38].copy_from_slice(&(data.len() as u32).to_be_bytes());
    rec[38..42].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    rec[REC_HEADER_SIZE..].copy_from_slice(payload);
    // CRC over the whole record; the crc field itself is still zero here.
    let crc = crc32c::crc32c(&rec);
    rec[42..46].copy_from_slice(&crc.to_be_bytes());
    Ok(rec)
}

/// Validates the record at the start of `b` (which may extend past it) and
/// returns its header. It checks framing, flags, key canonicality, and the
/// CRC, without needing to mutate `b` (`b` may be a read-only mmap).
pub fn parse_record(b: &[u8]) -> Result<Record, Error> {
    if b.len() < REC_HEADER_SIZE {
        return Err(Error::Corrupt("truncated record header".into()));
    }
    if b[0] != TAG_CHUNK {
        return Err(Error::Corrupt(format!("unexpected record tag {:#x}", b[0])));
    }
    let flags = b[33];
    if flags & !FLAG_ZSTD != 0 {
        return Err(Error::Corrupt(format!("unknown record flags {flags:#x}")));
    }
    let ulen = u32::from_be_bytes([b[34], b[35], b[36], b[37]]);
    let slen = u32::from_be_bytes([b[38], b[39], b[40], b[41]]);
    if (b.len() as u64) < REC_HEADER_SIZE as u64 + u64::from(slen) {
        return Err(Error::Corrupt("truncated record payload".into()));
    }
    if flags & FLAG_ZSTD == 0 && ulen != slen {
        return Err(Error::Corrupt(format!(
            "raw record with ulen {ulen} != slen {slen}"
        )));
    }
    if ulen > MAX_PAYLOAD {
        return Err(Error::Corrupt(format!(
            "record ulen {ulen} exceeds limit {MAX_PAYLOAD}"
        )));
    }
    if flags & FLAG_ZSTD != 0 && slen >= ulen {
        return Err(Error::Corrupt(format!(
            "compressed record with slen {slen} >= ulen {ulen}"
        )));
    }
    let mut c = crc32c::crc32c(&b[..42]);
    c = crc32c::crc32c_append(c, &[0u8; 4]);
    c = crc32c::crc32c_append(c, &b[REC_HEADER_SIZE..REC_HEADER_SIZE + slen as usize]);
    if c != u32::from_be_bytes([b[42], b[43], b[44], b[45]]) {
        return Err(Error::Corrupt("record CRC mismatch".into()));
    }
    let key = Key::parse(&b[1..33]).map_err(|e| Error::Corrupt(format!("record key: {e}")))?;
    Ok(Record {
        key,
        flags,
        ulen,
        slen,
    })
}

/// Returns caller-owned payload bytes from a record's stored payload. `stored`
/// may be a read-only mmap slice and is never retained.
pub fn decode_payload(flags: u8, ulen: u32, stored: &[u8]) -> Result<Vec<u8>, Error> {
    if flags & FLAG_ZSTD == 0 {
        return Ok(stored.to_vec());
    }
    // The decompression buffer is capped at ulen, so a frame that would expand
    // past the header's claim fails inside zstd rather than allocating; either
    // way the record is Corrupt (Go decodes fully, then reports the length
    // mismatch — same class, slightly different message in that edge).
    let out = zstd::bulk::decompress(stored, ulen as usize)
        .map_err(|e| Error::Corrupt(format!("zstd: {e}")))?;
    if out.len() != ulen as usize {
        return Err(Error::Corrupt(format!(
            "decompressed to {} bytes, header says {}",
            out.len(),
            ulen
        )));
    }
    Ok(out)
}

/// Serializes objects into the wire pack format. It is not safe for concurrent
/// use; a client wanting parallel uploads creates one `Writer` per pack.
///
/// Go's `Writer.Add(fstree.Object)` maps to [`Writer::add`]`(key, bytes)`.
pub struct Writer<W: Write> {
    bw: BufWriter<W>,
    wrote_header: bool,
}

impl<W: Write> Writer<W> {
    /// Returns a `Writer` emitting to `w`. The caller owns `w`;
    /// [`Writer::finish`] only writes the end marker, flushes, and hands `w`
    /// back.
    pub fn new(w: W) -> Writer<W> {
        Writer {
            bw: BufWriter::new(w),
            wrote_header: false,
        }
    }

    fn ensure_header(&mut self) -> Result<(), Error> {
        if self.wrote_header {
            return Ok(());
        }
        self.bw.write_all(PACK_MAGIC)?;
        self.wrote_header = true;
        Ok(())
    }

    /// Appends one object record.
    pub fn add(&mut self, k: Key, data: &[u8]) -> Result<(), Error> {
        self.ensure_header()?;
        let rec = encode_record(k, data)?;
        self.bw.write_all(&rec)?;
        Ok(())
    }

    /// Appends a pre-encoded record (an [`encode_record`] output, as stored
    /// verbatim on disk) without decoding or re-encoding it. It is the
    /// zero-copy counterpart to [`Writer::add`]: the push path reads a record
    /// straight from the local store and writes it to the wire, skipping the
    /// decompress/recompress round trip. `rec` is written as given; its
    /// framing and CRC are validated by the receiving [`Reader`].
    pub fn add_record(&mut self, rec: &[u8]) -> Result<(), Error> {
        self.ensure_header()?;
        self.bw.write_all(rec)?;
        Ok(())
    }

    /// Writes the header (if no object was added) and the end marker, then
    /// flushes and returns the underlying writer (Go: `Close`; it does not
    /// close the destination).
    pub fn finish(mut self) -> Result<W, Error> {
        self.ensure_header()?;
        self.bw.write_all(&[TAG_END])?;
        self.bw.flush()?;
        self.bw.into_inner().map_err(|e| Error::Io(e.into_error()))
    }
}

/// One record of a pack as it was read: its parsed header and its complete
/// bytes, header and stored payload, exactly as [`encode_record`] produced
/// them. `bytes` is caller-owned (Go: `RawRecord`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawRecord {
    /// The parsed header (Go: the embedded `Record`).
    pub record: Record,
    /// The complete record bytes, as stored.
    pub bytes: Vec<u8>,
}

enum ReaderState {
    Magic,
    Records,
    Done,
}

/// Decodes a wire pack stream.
///
/// `Reader` is an iterator over `Result<(Key, Vec<u8>), Error>` (Go:
/// `Reader.All`, yielding `fstree.Object`s). It yields exactly one error (and
/// then stops) on any structural problem; on a clean stream it yields every
/// object and finishes after the end marker. [`Reader::records`] is the same
/// stream with every record left undecoded.
pub struct Reader<R: Read> {
    br: BufReader<R>,
    state: ReaderState,
}

impl<R: Read> Reader<R> {
    /// Returns a `Reader` over `r`.
    pub fn new(r: R) -> Reader<R> {
        Reader {
            br: BufReader::new(r),
            state: ReaderState::Magic,
        }
    }

    fn read_full(&mut self, buf: &mut [u8], what: &str) -> Result<(), Error> {
        self.br
            .read_exact(buf)
            .map_err(|e| Error::Malformed(format!("{what}: {e}")))
    }

    /// Reads the next record undecoded, `Ok(None)` on the end marker.
    fn next_record(&mut self) -> Result<Option<RawRecord>, Error> {
        if matches!(self.state, ReaderState::Magic) {
            let mut magic = [0u8; PACK_MAGIC.len()];
            self.read_full(&mut magic, "reading magic")?;
            if &magic != PACK_MAGIC {
                return Err(Error::Malformed("bad magic".into()));
            }
            self.state = ReaderState::Records;
        }
        let mut tag = [0u8; 1];
        self.read_full(&mut tag, "truncated before end marker")?;
        match tag[0] {
            TAG_END => Ok(None),
            TAG_CHUNK => {
                // Reassemble the full record — tag + remaining 45 header bytes
                // + slen payload bytes — then validate it with parse_record.
                let mut hdr = [0u8; REC_HEADER_SIZE];
                hdr[0] = TAG_CHUNK;
                {
                    let (_, rest) = hdr.split_at_mut(1);
                    self.read_full(rest, "truncated record header")?;
                }
                let slen = u32::from_be_bytes([hdr[38], hdr[39], hdr[40], hdr[41]]);
                if slen > MAX_PAYLOAD {
                    return Err(Error::Malformed(format!(
                        "record payload {slen} exceeds limit {MAX_PAYLOAD}"
                    )));
                }
                let mut full = vec![0u8; REC_HEADER_SIZE + slen as usize];
                full[..REC_HEADER_SIZE].copy_from_slice(&hdr);
                {
                    let (_, payload) = full.split_at_mut(REC_HEADER_SIZE);
                    self.read_full(payload, "truncated record payload")?;
                }
                let rec = parse_record(&full).map_err(|e| Error::Malformed(e.to_string()))?;
                Ok(Some(RawRecord {
                    record: rec,
                    bytes: full,
                }))
            }
            t => Err(Error::Malformed(format!("bad record tag {t:#x}"))),
        }
    }

    /// Reads the next object, `Ok(None)` on the end marker: the next record
    /// with its payload decoded.
    fn next_object(&mut self) -> Result<Option<(Key, Vec<u8>)>, Error> {
        let Some(raw) = self.next_record()? else {
            return Ok(None);
        };
        let payload = decode_payload(
            raw.record.flags,
            raw.record.ulen,
            &raw.bytes[REC_HEADER_SIZE..],
        )
        .map_err(|e| Error::Malformed(e.to_string()))?;
        Ok(Some((raw.record.key, payload)))
    }

    /// One fused iterator step: `read` runs unless the reader is already
    /// done, and the end marker or an error finishes it, so at most one
    /// error is ever yielded.
    fn step<T>(
        &mut self,
        read: impl FnOnce(&mut Self) -> Result<Option<T>, Error>,
    ) -> Option<Result<T, Error>> {
        if matches!(self.state, ReaderState::Done) {
            return None;
        }
        match read(self) {
            Ok(Some(v)) => Some(Ok(v)),
            Ok(None) => {
                self.state = ReaderState::Done;
                None
            }
            Err(e) => {
                self.state = ReaderState::Done;
                Some(Err(e))
            }
        }
    }

    /// Iterates over the records in the stream without decoding them. Every
    /// record is validated exactly as the decoding iterator validates it
    /// (framing, the size bound, CRC, key canonicality) but its payload stays
    /// as stored, so a consumer that appends records verbatim (a packstore
    /// `Object` offered with its `record` set, [`Writer::add_record`]) skips
    /// the decompress/recompress round trip; it is the read-side counterpart
    /// of `add_record`. The iterator yields exactly one error (and then
    /// stops) on any structural problem; on a clean stream it yields every
    /// record and finishes after the end marker. It consumes the `Reader`,
    /// so the stream is read once whichever way it is consumed; records the
    /// decoding iterator already yielded are not re-read (Go:
    /// `Reader.Records`).
    pub fn records(self) -> Records<R> {
        Records { reader: self }
    }
}

impl<R: Read> Iterator for Reader<R> {
    type Item = Result<(Key, Vec<u8>), Error>;

    fn next(&mut self) -> Option<Self::Item> {
        self.step(Reader::next_object)
    }
}

/// The iterator [`Reader::records`] returns: each validated record of the
/// stream, undecoded, as a [`RawRecord`].
pub struct Records<R: Read> {
    reader: Reader<R>,
}

impl<R: Read> Iterator for Records<R> {
    type Item = Result<RawRecord, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        self.reader.step(Reader::next_record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::Type;

    /// n deterministic pseudo-random bytes (zstd cannot shrink them). The Go
    /// test uses a PCG stream; the property, not the exact bytes, matters.
    fn incompressible(n: usize) -> Vec<u8> {
        let mut state: u64 = 0x1234_5678_9abc_def0;
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

    /// n highly repetitive bytes (zstd shrinks them a lot).
    fn compressible(n: usize) -> Vec<u8> {
        b"abcdefgh".iter().copied().cycle().take(n).collect()
    }

    /// A canonical Blob key for data (Go mkObj: fstree.EncodeBlob — a Blob's
    /// serialized bytes are the raw data, its logical length the byte length).
    fn mk_key(data: &[u8]) -> Key {
        Key::new(Type::Blob, data.len() as u64, data)
    }

    /// Recomputes a record's CRC after test tampering.
    fn fix_crc(rec: &mut [u8]) {
        rec[42..46].copy_from_slice(&[0; 4]);
        let crc = crc32c::crc32c(rec);
        rec[42..46].copy_from_slice(&crc.to_be_bytes());
    }

    /// Reads the ulen field of a record.
    fn r0ulen(rec: &[u8]) -> u32 {
        u32::from_be_bytes([rec[34], rec[35], rec[36], rec[37]])
    }

    fn wire_pack(body: &[u8]) -> Vec<u8> {
        let mut v = PACK_MAGIC.to_vec();
        v.extend_from_slice(body);
        v
    }

    fn collect<R: Read>(r: Reader<R>) -> Result<Vec<(Key, Vec<u8>)>, Error> {
        let mut out = Vec::new();
        for item in r {
            out.push(item?);
        }
        Ok(out)
    }

    #[test]
    fn record_round_trip_raw() {
        let data = incompressible(4096);
        let k = mk_key(&data);
        let rec = encode_record(k, &data).unwrap();
        let r = parse_record(&rec).unwrap();
        assert_eq!(r.key, k, "key mismatch");
        assert_eq!(r.flags, 0, "random data must be stored raw");
        assert_eq!(r.ulen, r.slen);
        assert_eq!(r.slen as usize, data.len());
        let got = decode_payload(
            r.flags,
            r.ulen,
            &rec[REC_HEADER_SIZE..REC_HEADER_SIZE + r.slen as usize],
        )
        .unwrap();
        assert_eq!(got, data, "payload mismatch");
    }

    #[test]
    fn record_round_trip_compressed() {
        let data = compressible(64 << 10);
        let k = mk_key(&data);
        let rec = encode_record(k, &data).unwrap();
        let r = parse_record(&rec).unwrap();
        assert_eq!(r.flags, FLAG_ZSTD, "repetitive data must compress");
        assert!(r.slen < r.ulen, "compressed slen must be < ulen");
        let got = decode_payload(
            r.flags,
            r.ulen,
            &rec[REC_HEADER_SIZE..REC_HEADER_SIZE + r.slen as usize],
        )
        .unwrap();
        assert_eq!(got, data, "payload mismatch after decompression");
    }

    #[test]
    fn record_empty_payload() {
        let k = mk_key(&[]);
        let rec = encode_record(k, &[]).unwrap();
        let r = parse_record(&rec).unwrap();
        assert_eq!((r.ulen, r.slen, r.flags), (0, 0, 0));
    }

    #[test]
    fn record_too_large_bound() {
        assert!(
            payload_fits(MAX_PAYLOAD as usize),
            "payload_fits must accept exactly MAX_PAYLOAD"
        );
        assert!(
            !payload_fits(MAX_PAYLOAD as usize + 1),
            "payload_fits must reject MAX_PAYLOAD+1"
        );
        assert!(payload_fits(0), "payload_fits must accept empty payloads");
    }

    #[test]
    fn parse_record_rejects_oversized_ulen() {
        let data = compressible(4096);
        let mut rec = encode_record(mk_key(&data), &data).unwrap();
        assert_eq!(
            rec[33] & FLAG_ZSTD,
            FLAG_ZSTD,
            "test needs a compressed record"
        );
        rec[34..38].copy_from_slice(&u32::MAX.to_be_bytes());
        fix_crc(&mut rec);
        let err = parse_record(&rec).unwrap_err();
        assert!(err.is_corrupt(), "want Corrupt, got {err:?}");
    }

    // Go's "bomb stops at ulen": a frame that inflates far past the declared
    // ulen must fail without allocating the inflated size. The Rust decoder
    // caps its buffer at ulen, so the 64 MiB expansion never materializes.
    #[test]
    fn decode_payload_bomb_stops_at_ulen() {
        let bomb =
            zstd::bulk::compress(&vec![0u8; 64 << 20], zstd::DEFAULT_COMPRESSION_LEVEL).unwrap();
        let err = decode_payload(FLAG_ZSTD, 1024, &bomb).unwrap_err();
        assert!(err.is_corrupt(), "want Corrupt, got {err:?}");
    }

    #[test]
    fn parse_record_rejects_corruption() {
        let data = incompressible(1024);
        let k = mk_key(&data);
        let rec = encode_record(k, &data).unwrap();

        // truncated header
        let err = parse_record(&rec[..REC_HEADER_SIZE - 1]).unwrap_err();
        assert!(err.is_corrupt(), "truncated header: {err}");

        // truncated payload
        let err = parse_record(&rec[..rec.len() - 1]).unwrap_err();
        assert!(err.is_corrupt(), "truncated payload: {err}");

        // bad tag
        let mut bad = rec.clone();
        bad[0] = 0x7F;
        let err = parse_record(&bad).unwrap_err();
        assert!(err.is_corrupt(), "bad tag: {err}");
        assert_eq!(
            err.to_string(),
            "amberpack: corrupt pack data: unexpected record tag 0x7f"
        );

        // bad flags
        let mut bad = rec.clone();
        bad[33] = 0x80;
        let err = parse_record(&bad).unwrap_err();
        assert!(err.is_corrupt(), "bad flags: {err}");
        assert_eq!(
            err.to_string(),
            "amberpack: corrupt pack data: unknown record flags 0x80"
        );

        // bad flags with the zstd bit also set: the mask must reject on the
        // unknown bit and the message prints the whole flag byte (Go %#x).
        let mut bad = rec.clone();
        bad[33] = 0x03;
        let err = parse_record(&bad).unwrap_err();
        assert!(err.is_corrupt(), "bad flags 0x03: {err}");
        assert_eq!(
            err.to_string(),
            "amberpack: corrupt pack data: unknown record flags 0x3"
        );

        // flipped payload byte fails CRC
        let mut bad = rec.clone();
        let last = bad.len() - 1;
        bad[last] ^= 0x01;
        let err = parse_record(&bad).unwrap_err();
        assert!(err.is_corrupt(), "flipped payload byte: {err}");
        assert_eq!(
            err.to_string(),
            "amberpack: corrupt pack data: record CRC mismatch"
        );

        // flipped length byte (inside slen, keeps record long enough to parse)
        let mut bad = rec.clone();
        bad[39] ^= 0x01;
        let err = parse_record(&bad).unwrap_err();
        assert!(err.is_corrupt(), "flipped length: {err}");

        // non-canonical key: encode with an invalid type nibble; encode_record
        // does not validate keys (callers supply canonical keys), parse_record
        // must.
        let mut kk = k;
        kk.0[0] = 0xF0; // type 15: reserved
        let bad = encode_record(kk, &data).unwrap();
        let err = parse_record(&bad).unwrap_err();
        assert!(err.is_corrupt(), "non-canonical key: {err}");
        assert_eq!(
            err.to_string(),
            "amberpack: corrupt pack data: record key: key: reserved object type: 15"
        );

        // raw ulen != slen
        let mut bad = rec.clone();
        let u = r0ulen(&bad) + 1;
        bad[34..38].copy_from_slice(&u.to_be_bytes());
        fix_crc(&mut bad);
        let err = parse_record(&bad).unwrap_err();
        assert!(err.is_corrupt(), "raw ulen != slen: {err}");

        // compressed slen >= ulen (constructed: flags=zstd, slen == ulen)
        let cdata = compressible(4096);
        let mut bad = encode_record(mk_key(&cdata), &cdata).unwrap();
        assert_eq!(bad[33], FLAG_ZSTD);
        let s = u32::from_be_bytes([bad[38], bad[39], bad[40], bad[41]]);
        bad[34..38].copy_from_slice(&s.to_be_bytes()); // ulen := slen
        fix_crc(&mut bad);
        let err = parse_record(&bad).unwrap_err();
        assert!(err.is_corrupt(), "compressed slen >= ulen: {err}");
    }

    // parse_record is called on mmap slices and tail-scan buffers that extend
    // past the current record; trailing bytes must not affect the parse result
    // or the CRC.
    #[test]
    fn parse_record_ignores_trailing_bytes() {
        let data = incompressible(512);
        let k = mk_key(&data);
        let rec = encode_record(k, &data).unwrap();
        let mut extended = rec.clone();
        extended.extend_from_slice(&incompressible(1000));
        let r = parse_record(&extended).unwrap();
        assert_eq!(r.key, k);
        assert_eq!(r.slen as usize, data.len());
    }

    #[test]
    fn decode_payload_errors() {
        // bad zstd frame
        let err = decode_payload(FLAG_ZSTD, 100, b"not a zstd frame").unwrap_err();
        assert!(err.is_corrupt(), "bad zstd frame: {err}");

        // ulen mismatch: an 11-byte payload claimed as 5
        let comp = zstd::bulk::compress(b"hello world", zstd::DEFAULT_COMPRESSION_LEVEL).unwrap();
        let err = decode_payload(FLAG_ZSTD, 5, &comp).unwrap_err();
        assert!(err.is_corrupt(), "ulen mismatch: {err}");

        // under-expansion (frame decodes to fewer bytes than ulen): Go's exact
        // message, byte-identical (verified differentially against Go).
        let err = decode_payload(FLAG_ZSTD, 20, &comp).unwrap_err();
        assert_eq!(
            err.to_string(),
            "amberpack: corrupt pack data: decompressed to 11 bytes, header says 20"
        );

        // empty stored bytes with ulen > 0: libzstd decodes zero frames to
        // zero bytes without error (like Go's klauspost), so the length check
        // reports it — pinning that the message stays byte-identical to Go.
        let err = decode_payload(FLAG_ZSTD, 5, &[]).unwrap_err();
        assert!(err.is_corrupt(), "empty stored: {err}");
        assert_eq!(
            err.to_string(),
            "amberpack: corrupt pack data: decompressed to 0 bytes, header says 5"
        );
    }

    #[test]
    fn decode_payload_raw_copies() {
        // Go guards against aliasing an mmap slice; the Rust raw path likewise
        // hands back an owned copy.
        let stored = [1u8, 2, 3, 4];
        let mut out = decode_payload(0, 4, &stored).unwrap();
        out[0] = 99;
        assert_eq!(stored[0], 1);
    }

    #[test]
    fn too_large_error_message() {
        let k = mk_key(b"x");
        let err = Error::TooLarge {
            key: k,
            len: 5_000_000_000,
        };
        assert_eq!(
            err.to_string(),
            format!("amberpack: object {k} too large: 5000000000 bytes")
        );
    }

    // ---- wire pack ----

    #[test]
    fn writer_reader_round_trip() {
        let payloads: Vec<Vec<u8>> = vec![b"alpha".to_vec(), Vec::new(), vec![b'x'; 5000]];
        let objs: Vec<(Key, Vec<u8>)> = payloads.into_iter().map(|p| (mk_key(&p), p)).collect();
        let mut w = Writer::new(Vec::new());
        for (k, p) in &objs {
            w.add(*k, p).unwrap();
        }
        let buf = w.finish().unwrap();
        let got = collect(Reader::new(&buf[..])).unwrap();
        assert_eq!(got, objs);
    }

    #[test]
    fn round_trip_compressed() {
        // A large, highly compressible payload: its per-record zstd makes the
        // pack clearly smaller than the raw bytes, proving compression is
        // applied.
        let big: Vec<u8> = b"amber".iter().copied().cycle().take(250_000).collect();
        let objs: Vec<(Key, Vec<u8>)> = vec![
            (mk_key(b"alpha"), b"alpha".to_vec()),
            (mk_key(&big), big.clone()),
        ];
        let mut w = Writer::new(Vec::new());
        for (k, p) in &objs {
            w.add(*k, p).unwrap();
        }
        let out = w.finish().unwrap();
        assert!(out.starts_with(PACK_MAGIC), "magic missing");
        assert!(
            out.len() < big.len(),
            "output {} bytes not smaller than raw payload {}; compression not applied",
            out.len(),
            big.len()
        );
        let got = collect(Reader::new(&out[..])).unwrap();
        assert_eq!(got, objs);
    }

    #[test]
    fn add_record_round_trip() {
        // add_record writes a pre-encoded record verbatim. Feeding it the
        // exact bytes encode_record produces must yield a stream the Reader
        // decodes identically to one built with add — the zero-copy push path.
        let big: Vec<u8> = b"amber".iter().copied().cycle().take(250_000).collect();
        let objs: Vec<(Key, Vec<u8>)> =
            vec![(mk_key(b"alpha"), b"alpha".to_vec()), (mk_key(&big), big)];
        let mut w = Writer::new(Vec::new());
        for (k, p) in &objs {
            let rec = encode_record(*k, p).unwrap();
            w.add_record(&rec).unwrap();
        }
        let buf = w.finish().unwrap();
        let got = collect(Reader::new(&buf[..])).unwrap();
        assert_eq!(got, objs);
    }

    #[test]
    fn reader_rejects_legacy_versions() {
        for magic in [b"AMBERPK\x01", b"AMBERPK\x02"] {
            let mut buf = magic.to_vec();
            buf.push(TAG_END);
            let err = collect(Reader::new(&buf[..])).unwrap_err();
            assert!(err.is_malformed(), "magic {magic:?}: {err}");
            assert_eq!(
                err.to_string(),
                "amberpack: malformed pack stream: bad magic"
            );
        }
    }

    #[test]
    fn empty_stream_is_valid() {
        let w = Writer::new(Vec::new());
        let buf = w.finish().unwrap();
        assert_eq!(buf, b"AMBERPK\x03\x00");
        let got = collect(Reader::new(&buf[..])).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn reader_bad_magic() {
        let err = collect(Reader::new(&b"NOTAMBER..."[..])).unwrap_err();
        assert!(err.is_malformed(), "{err}");
    }

    #[test]
    fn reader_truncated_missing_end_marker() {
        // One complete record but no end marker: the loop reads the record,
        // then hits EOF where the end marker (or next tag) should be.
        let rec = encode_record(mk_key(b"data"), b"data").unwrap();
        let err = collect(Reader::new(&wire_pack(&rec)[..])).unwrap_err();
        assert!(err.is_malformed(), "missing end marker: {err}");
    }

    #[test]
    fn reader_non_canonical_key_rejected() {
        // A record whose key has the reserved type nibble set, so Key::parse
        // fails in parse_record. encode_record writes the key as given.
        let mut k = mk_key(b"payload");
        k.0[0] = 0xF0; // reserved type nibble -> Key::parse fails
        let mut body = encode_record(k, b"payload").unwrap();
        body.push(TAG_END);
        let err = collect(Reader::new(&wire_pack(&body)[..])).unwrap_err();
        assert!(err.is_malformed(), "bad key: {err}");
    }

    #[test]
    fn reader_bad_record_tag() {
        let err = collect(Reader::new(&wire_pack(&[0x42])[..])).unwrap_err();
        assert!(err.is_malformed(), "bad record tag: {err}");
        assert_eq!(
            err.to_string(),
            "amberpack: malformed pack stream: bad record tag 0x42"
        );
    }

    #[test]
    fn reader_truncated_payload() {
        // A record header claims a 100-byte payload but the stream ends after 5.
        let data = incompressible(100); // incompressible -> stored raw, slen = 100
        let rec = encode_record(mk_key(&data), &data).unwrap();
        let truncated = &rec[..REC_HEADER_SIZE + 5];
        let err = collect(Reader::new(&wire_pack(truncated)[..])).unwrap_err();
        assert!(err.is_malformed(), "truncated payload: {err}");
    }

    #[test]
    fn reader_record_crc_mismatch() {
        // A flipped payload byte fails the record CRC inside parse_record.
        // Exactly like Go's `%w: %v` wrapping, the reader classifies it as
        // Malformed (not Corrupt) with the corrupt-record text embedded.
        let data = incompressible(64);
        let mut rec = encode_record(mk_key(&data), &data).unwrap();
        let last = rec.len() - 1;
        rec[last] ^= 0x01;
        rec.push(TAG_END);
        let err = collect(Reader::new(&wire_pack(&rec)[..])).unwrap_err();
        assert!(err.is_malformed(), "record CRC mismatch: {err}");
        assert!(
            !err.is_corrupt(),
            "stream errors are Malformed, like Go errors.Is"
        );
        assert_eq!(
            err.to_string(),
            "amberpack: malformed pack stream: amberpack: corrupt pack data: record CRC mismatch"
        );
    }

    #[test]
    fn reader_oversized_payload_rejected() {
        // A header claiming a payload above MAX_PAYLOAD is rejected
        // before any allocation (and before the CRC check). Only the header
        // follows the magic — no payload bytes — so the size guard must fire
        // before the payload read, not after.
        let rec = encode_record(mk_key(b"x"), b"x").unwrap();
        let mut hdr = rec[..REC_HEADER_SIZE].to_vec();
        hdr[38..42].copy_from_slice(&(MAX_PAYLOAD + 1).to_be_bytes());
        let err = collect(Reader::new(&wire_pack(&hdr)[..])).unwrap_err();
        assert!(err.is_malformed(), "oversized payload: {err}");
        assert!(
            err.to_string().contains("exceeds limit"),
            "guard must fire before the payload read: {err}"
        );
    }

    #[test]
    fn reader_stops_at_end_marker_ignoring_trailing_bytes() {
        // Go's All returns at tagEnd without touching what follows; bytes
        // after the end marker (e.g. concatenated data) must not affect the
        // decode or produce an error.
        let rec = encode_record(mk_key(b"data"), b"data").unwrap();
        let mut body = rec;
        body.push(TAG_END);
        body.extend_from_slice(b"\xFFtrailing garbage");
        let got = collect(Reader::new(&wire_pack(&body)[..])).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1, b"data");
    }

    #[test]
    fn reader_stops_after_error() {
        // The iterator yields exactly one error and then fuses, mirroring
        // Go's All yielding once and returning.
        let mut r = Reader::new(&b"NOTAMBER..."[..]);
        assert!(matches!(r.next(), Some(Err(_))));
        assert!(r.next().is_none());
    }

    /// Drains `records`, returning what it yielded and the error that ended
    /// it (Go: `collectRecords`).
    fn collect_records<R: Read>(r: Records<R>) -> (Vec<RawRecord>, Option<Error>) {
        let mut out = Vec::new();
        for item in r {
            match item {
                Ok(rec) => out.push(rec),
                Err(e) => return (out, Some(e)),
            }
        }
        (out, None)
    }

    #[test]
    fn reader_records_round_trip() {
        // records yields every record's bytes exactly as encode_record
        // produced them, with its parsed header, and without decoding: a
        // consumer that appends records verbatim (packstore's Object.record)
        // never touches zstd. Feeding those bytes back through add_record
        // must give a stream the decoding iterator reads as the original
        // objects.
        let big: Vec<u8> = b"amber".iter().copied().cycle().take(250_000).collect();
        let noise = incompressible(4000);
        let objs: Vec<(Key, Vec<u8>)> = vec![
            (mk_key(b"alpha"), b"alpha".to_vec()),
            (mk_key(b""), Vec::new()),
            (mk_key(&big), big), // compressed on disk
            (mk_key(&noise), noise),
        ];
        let mut w = Writer::new(Vec::new());
        let mut want = Vec::new();
        for (i, (k, p)) in objs.iter().enumerate() {
            let rec = encode_record(*k, p).unwrap();
            if i % 2 == 0 {
                w.add(*k, p).unwrap();
            } else {
                w.add_record(&rec).unwrap();
            }
            want.push(rec);
        }
        let buf = w.finish().unwrap();

        let (got, err) = collect_records(Reader::new(&buf[..]).records());
        assert!(err.is_none(), "records: {err:?}");
        assert_eq!(got.len(), objs.len(), "record count");
        let mut w2 = Writer::new(Vec::new());
        for (i, rec) in got.iter().enumerate() {
            assert_eq!(
                rec.bytes, want[i],
                "record {i} bytes differ from encode_record's"
            );
            assert_eq!(rec.record.key, objs[i].0, "record {i} key");
            assert_eq!(rec.record.ulen as usize, objs[i].1.len(), "record {i} ulen");
            assert_eq!(
                rec.bytes.len(),
                REC_HEADER_SIZE + rec.record.slen as usize,
                "record {i} length disagrees with its header"
            );
            w2.add_record(&rec.bytes).unwrap();
        }
        let again = w2.finish().unwrap();
        let decoded = collect(Reader::new(&again[..])).unwrap();
        assert_eq!(decoded, objs, "objects differ after the record round trip");
    }

    #[test]
    fn reader_records_truncated() {
        // A stream cut before the end marker is malformed for records exactly
        // as it is for the decoding iterator.
        let rec = encode_record(mk_key(b"alpha"), b"alpha").unwrap();
        let (got, err) = collect_records(Reader::new(&wire_pack(&rec)[..]).records());
        let err = err.expect("want an error");
        assert!(err.is_malformed(), "err = {err}, want malformed");
        assert_eq!(
            got.len(),
            1,
            "yielded {} records before the truncation, want 1",
            got.len()
        );
    }

    #[test]
    fn reader_records_crc_mismatch() {
        let data = incompressible(64);
        let mut rec = encode_record(mk_key(&data), &data).unwrap();
        let last = rec.len() - 1;
        rec[last] ^= 0x01;
        rec.push(TAG_END);
        let (_, err) = collect_records(Reader::new(&wire_pack(&rec)[..]).records());
        let err = err.expect("want an error");
        assert!(
            err.is_malformed(),
            "err = {err}, want malformed (record CRC mismatch)"
        );
    }

    #[test]
    fn records_stops_after_error() {
        // Like the decoding iterator, records yields exactly one error and
        // then fuses.
        let mut r = Reader::new(&b"NOTAMBER..."[..]).records();
        assert!(matches!(r.next(), Some(Err(_))));
        assert!(r.next().is_none());
    }
}
