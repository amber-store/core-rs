//! Minimal canonical-CBOR encoding helpers shared by the tree and reference
//! codecs (Go package `cborx`).
//!
//! The Go implementation exists because fxamacker/cbor cannot emit
//! byte-string-keyed maps from a `map[string][]byte`; this port hand-rolls the
//! same primitives and additionally exposes them (`append_head`, `read_head`,
//! `append_bstr`, `read_bstr`) as `pub` so the `fstree` and `reference`
//! modules can reuse them. Output follows RFC 8949 section 4.2 core
//! deterministic encoding.
//!
//! Encoding always emits shortest-form heads and definite lengths. Decoding
//! mirrors the Go reader exactly, including its laxness: `read_head` accepts
//! *any* of the five head forms (so a non-shortest head such as `0x18 0x05`
//! for the value 5 is accepted), and rejects only additional info 28–31
//! (reserved / indefinite-length). See [`read_head`].

use std::collections::BTreeMap;
use std::fmt;

/// CBOR major type 0: unsigned integer.
pub const MAJOR_UINT: u8 = 0;
/// CBOR major type 1: negative integer.
pub const MAJOR_NEGINT: u8 = 1;
/// CBOR major type 2: byte string.
pub const MAJOR_BSTR: u8 = 2;
/// CBOR major type 3: text string.
pub const MAJOR_TSTR: u8 = 3;
/// CBOR major type 4: array.
pub const MAJOR_ARRAY: u8 = 4;
/// CBOR major type 5: map.
pub const MAJOR_MAP: u8 = 5;

/// Decoding error for this module's readers.
///
/// Mirrors the Go package's error values: `UnexpectedEof` stands in for
/// `io.ErrUnexpectedEOF`, the remaining variants carry the same diagnostic
/// content as the corresponding `fmt.Errorf` messages, and the two `Xattr*`
/// variants reproduce Go's error wrapping (`%w`) with a boxed source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The input ended before a complete item was read.
    UnexpectedEof,
    /// A head byte carried additional info 28–31, which this codec never
    /// emits (reserved values and indefinite-length markers).
    UnsupportedAdditionalInfo(u8),
    /// Expected a CBOR map (major type 5).
    ExpectedMap {
        /// The major type actually found.
        got: u8,
    },
    /// Expected a CBOR byte string (major type 2).
    ExpectedByteString {
        /// The major type actually found.
        got: u8,
    },
    /// Input remained after the complete xattr map.
    TrailingBytes(usize),
    /// The map head claimed more pairs than the remaining bytes could hold.
    PairCount {
        /// The claimed pair count.
        pairs: u64,
        /// The bytes remaining after the map head.
        bytes: usize,
    },
    /// Reading the `index`-th xattr key failed.
    XattrKey {
        /// Zero-based pair index within the map.
        index: u64,
        /// The underlying decode error.
        source: Box<Error>,
    },
    /// Reading the value for the key `name` failed.
    XattrValue {
        /// The already-decoded key whose value was bad.
        name: Vec<u8>,
        /// The underlying decode error.
        source: Box<Error>,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::UnexpectedEof => write!(f, "cbor: unexpected EOF"),
            Error::UnsupportedAdditionalInfo(ai) => {
                write!(f, "cbor: unsupported additional info {ai}")
            }
            Error::ExpectedMap { got } => {
                write!(f, "cbor: expected CBOR map (major 5), got major {got}")
            }
            Error::ExpectedByteString { got } => {
                write!(f, "cbor: expected byte string (major 2), got major {got}")
            }
            Error::TrailingBytes(n) => {
                write!(f, "cbor: {n} trailing bytes after xattr map")
            }
            Error::PairCount { pairs, bytes } => {
                write!(f, "cbor: xattr map claims {pairs} pairs in {bytes} bytes")
            }
            Error::XattrKey { index, source } => {
                write!(f, "cbor: xattr key {index}: {source}")
            }
            Error::XattrValue { name, source } => {
                write!(
                    f,
                    "cbor: xattr value for {:?}: {source}",
                    String::from_utf8_lossy(name)
                )
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::XattrKey { source, .. } | Error::XattrValue { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Appends a CBOR head (major type in the high 3 bits) carrying the argument
/// `n` in the shortest form, per RFC 8949 section 4.2.
pub fn append_head(b: &mut Vec<u8>, major: u8, n: u64) {
    let h = major << 5;
    if n < 24 {
        b.push(h | n as u8);
    } else if n < 1 << 8 {
        b.push(h | 24);
        b.push(n as u8);
    } else if n < 1 << 16 {
        b.push(h | 25);
        b.extend_from_slice(&(n as u16).to_be_bytes());
    } else if n < 1 << 32 {
        b.push(h | 26);
        b.extend_from_slice(&(n as u32).to_be_bytes());
    } else {
        b.push(h | 27);
        b.extend_from_slice(&n.to_be_bytes());
    }
}

/// Appends `s` as a CBOR text string (major type 3).
pub fn append_tstr(b: &mut Vec<u8>, s: &str) {
    append_head(b, MAJOR_TSTR, s.len() as u64);
    b.extend_from_slice(s.as_bytes());
}

/// Appends `v` as a CBOR integer in the shortest form: major type 0 when
/// non-negative, major type 1 otherwise.
pub fn append_int(b: &mut Vec<u8>, v: i64) {
    if v >= 0 {
        append_head(b, MAJOR_UINT, v as u64);
    } else {
        // Major 1 carries -1 - n; for negative v that argument is the bitwise
        // complement, which also holds for i64::MIN.
        append_head(b, MAJOR_NEGINT, !(v as u64));
    }
}

/// Appends `s` as a CBOR byte string (major type 2).
pub fn append_bstr(b: &mut Vec<u8>, s: &[u8]) {
    append_head(b, MAJOR_BSTR, s.len() as u64);
    b.extend_from_slice(s);
}

/// Reads one CBOR head, returning its major type, argument, and the remaining
/// bytes.
///
/// Faithful to Go's `readHead`: every definite-length head form is accepted —
/// including non-shortest encodings such as `0x18 0x05` — even though
/// [`append_head`] only ever emits the shortest form. Only additional info
/// 28–31 is rejected.
pub fn read_head(b: &[u8]) -> Result<(u8, u64, &[u8]), Error> {
    let (&first, b) = b.split_first().ok_or(Error::UnexpectedEof)?;
    let major = first >> 5;
    let ai = first & 0x1f;
    match ai {
        0..=23 => Ok((major, u64::from(ai), b)),
        24 => {
            if b.is_empty() {
                return Err(Error::UnexpectedEof);
            }
            Ok((major, u64::from(b[0]), &b[1..]))
        }
        25 => {
            if b.len() < 2 {
                return Err(Error::UnexpectedEof);
            }
            let n = u16::from_be_bytes([b[0], b[1]]);
            Ok((major, u64::from(n), &b[2..]))
        }
        26 => {
            if b.len() < 4 {
                return Err(Error::UnexpectedEof);
            }
            let n = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
            Ok((major, u64::from(n), &b[4..]))
        }
        27 => {
            if b.len() < 8 {
                return Err(Error::UnexpectedEof);
            }
            let n = u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
            Ok((major, n, &b[8..]))
        }
        _ => Err(Error::UnsupportedAdditionalInfo(ai)),
    }
}

/// Reads one CBOR byte string, returning its contents (borrowed from the
/// input) and the remaining input.
///
/// Go's `readBStr` returns a fresh copy; here a subslice borrow is returned
/// instead, which is semantically equivalent for callers that copy what they
/// keep (as [`decode_xattrs`] does).
pub fn read_bstr(b: &[u8]) -> Result<(&[u8], &[u8]), Error> {
    let (major, n, b) = read_head(b)?;
    if major != MAJOR_BSTR {
        return Err(Error::ExpectedByteString { got: major });
    }
    if (b.len() as u64) < n {
        return Err(Error::UnexpectedEof);
    }
    let n = n as usize; // fits: n <= b.len()
    Ok((&b[..n], &b[n..]))
}

/// Encodes `m` as a canonical CBOR map with byte-string keys and values, keys
/// sorted by the bytewise lexicographic order of their CBOR encodings
/// (RFC 8949 section 4.2). Used both inline (DirLeaf key 8) and as the
/// XattrSet object body (key 9 target).
///
/// Keys are byte slices rather than Go's `string` because xattr names are not
/// guaranteed to be valid UTF-8; Go strings tolerate arbitrary bytes, Rust
/// `String` does not.
pub fn encode_xattrs(m: &BTreeMap<Vec<u8>, Vec<u8>>) -> Vec<u8> {
    let mut names: Vec<&[u8]> = m.keys().map(Vec::as_slice).collect();
    names.sort_by(|a, b| {
        let mut ea = Vec::new();
        append_bstr(&mut ea, a);
        let mut eb = Vec::new();
        append_bstr(&mut eb, b);
        ea.cmp(&eb)
    });
    let mut out = Vec::new();
    append_head(&mut out, MAJOR_MAP, m.len() as u64);
    for n in names {
        append_bstr(&mut out, n);
        append_bstr(&mut out, &m[n]);
    }
    out
}

/// Parses a canonical CBOR map of byte-string keys to byte-string values, the
/// inverse of [`encode_xattrs`]. It accepts only the definite-length forms
/// this module emits and rejects any trailing bytes.
///
/// Like Go's `DecodeXattrs` this inherits `read_head`'s tolerance of
/// non-shortest length heads, and a duplicate key silently keeps the last
/// value (Go map-insert semantics).
pub fn decode_xattrs(b: &[u8]) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, Error> {
    let (major, n, mut b) = read_head(b)?;
    if major != MAJOR_MAP {
        return Err(Error::ExpectedMap { got: major });
    }
    // n is untrusted. A pair needs at least 2 bytes.
    if n > b.len() as u64 / 2 {
        return Err(Error::PairCount {
            pairs: n,
            bytes: b.len(),
        });
    }
    let mut m = BTreeMap::new();
    for i in 0..n {
        let (name, rest) = read_bstr(b).map_err(|e| Error::XattrKey {
            index: i,
            source: Box::new(e),
        })?;
        let (val, rest) = read_bstr(rest).map_err(|e| Error::XattrValue {
            name: name.to_vec(),
            source: Box::new(e),
        })?;
        m.insert(name.to_vec(), val.to_vec());
        b = rest;
    }
    if !b.is_empty() {
        return Err(Error::TrailingBytes(b.len()));
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(major: u8, n: u64) -> Vec<u8> {
        let mut b = Vec::new();
        append_head(&mut b, major, n);
        b
    }

    fn bstr(s: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        append_bstr(&mut b, s);
        b
    }

    fn xmap(pairs: &[(&[u8], &[u8])]) -> BTreeMap<Vec<u8>, Vec<u8>> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_vec(), v.to_vec()))
            .collect()
    }

    // Port of Go TestAppendHead_ShortestForm, extended past 16-bit arguments.
    #[test]
    fn append_head_shortest_form() {
        let cases: &[(u8, u64, &[u8])] = &[
            (2, 0, &[0x40]),               // bstr, len 0
            (2, 5, &[0x45]),               // bstr, len 5
            (2, 23, &[0x57]),              // bstr, len 23 (last 1-byte head)
            (2, 24, &[0x58, 0x18]),        // bstr, len 24 (needs 1 length byte)
            (2, 255, &[0x58, 0xff]),       // bstr, len 255
            (2, 256, &[0x59, 0x01, 0x00]), // bstr, len 256
            (5, 2, &[0xa2]),               // map, 2 pairs
            (2, 65535, &[0x59, 0xff, 0xff]),
            (2, 65536, &[0x5a, 0x00, 0x01, 0x00, 0x00]),
            (2, 0xffff_ffff, &[0x5a, 0xff, 0xff, 0xff, 0xff]),
            (
                2,
                0x1_0000_0000,
                &[0x5b, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00],
            ),
            (
                0,
                u64::MAX,
                &[0x1b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
            ),
        ];
        for &(major, n, want) in cases {
            assert_eq!(head(major, n), want, "append_head({major},{n})");
        }
    }

    #[test]
    fn read_head_roundtrip() {
        for &n in &[
            0u64,
            1,
            23,
            24,
            255,
            256,
            65535,
            65536,
            0xffff_ffff,
            0x1_0000_0000,
            u64::MAX,
        ] {
            for major in 0u8..=7 {
                let mut b = head(major, n);
                b.push(0xee); // trailing byte must come back as rest
                let (m, got, rest) = read_head(&b).unwrap();
                assert_eq!(
                    (m, got, rest),
                    (major, n, &[0xee][..]),
                    "n={n} major={major}"
                );
            }
        }
    }

    // Go quirk preserved: readHead accepts any emitted head form, including
    // non-shortest ones. 0x18 0x05 (value 5 in a 1-byte-argument head) parses.
    #[test]
    fn read_head_accepts_non_shortest() {
        let (major, n, rest) = read_head(&[0x18, 0x05]).unwrap();
        assert_eq!((major, n, rest), (0, 5, &[][..]));
        let (major, n, rest) = read_head(&[0x59, 0x00, 0x01, 0x61]).unwrap();
        assert_eq!((major, n, rest), (2, 1, &[0x61][..]));
        let (major, n, rest) = read_head(&[0x1b, 0, 0, 0, 0, 0, 0, 0, 7]).unwrap();
        assert_eq!((major, n, rest), (0, 7, &[][..]));
    }

    #[test]
    fn read_head_rejects_ai_28_to_31() {
        for ai in 28u8..=31 {
            let b = [(2 << 5) | ai, 0x00];
            assert_eq!(read_head(&b), Err(Error::UnsupportedAdditionalInfo(ai)));
        }
        // 0x5f: indefinite-length byte string marker.
        assert_eq!(
            read_head(&[0x5f]),
            Err(Error::UnsupportedAdditionalInfo(31))
        );
    }

    #[test]
    fn read_head_truncated() {
        let cases: &[&[u8]] = &[
            &[],
            &[0x18],
            &[0x19, 0x01],
            &[0x1a, 0x01, 0x02, 0x03],
            &[0x1b, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07],
        ];
        for &c in cases {
            assert_eq!(read_head(c), Err(Error::UnexpectedEof), "input {c:02x?}");
        }
    }

    #[test]
    fn read_bstr_ok_and_errors() {
        let (val, rest) = read_bstr(&[0x43, b'a', b'b', b'c', 0x01]).unwrap();
        assert_eq!((val, rest), (&b"abc"[..], &[0x01][..]));
        let (val, rest) = read_bstr(&[0x40]).unwrap();
        assert_eq!((val, rest), (&b""[..], &[][..]));
        // Wrong major.
        assert_eq!(
            read_bstr(&[0xa0]),
            Err(Error::ExpectedByteString { got: 5 })
        );
        assert_eq!(
            read_bstr(&[0x05]),
            Err(Error::ExpectedByteString { got: 0 })
        );
        // Truncated payload.
        assert_eq!(read_bstr(&[0x45, 1, 2]), Err(Error::UnexpectedEof));
        // Claimed length far beyond input.
        let mut huge = Vec::new();
        append_head(&mut huge, 2, u64::MAX);
        assert_eq!(read_bstr(&huge), Err(Error::UnexpectedEof));
    }

    // Port of Go TestDecodeXattrs_RejectsOversizedCount.
    #[test]
    fn decode_xattrs_rejects_oversized_count() {
        // Headers claiming far more pairs than the remaining bytes could hold
        // must be rejected up front, before any per-pair work.
        for body in [
            &[0xba, 0xff, 0xff, 0xff, 0xff][..], // u32 count
            &[0xbb, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00], // u64 count
            &[0xb9, 0xff, 0xff, 0x40, 0x40],     // u16 count, 2 bytes left
        ] {
            match decode_xattrs(body) {
                Err(Error::PairCount { .. }) => {}
                other => panic!("{body:02x?}: got {other:?}, want PairCount error"),
            }
        }
    }

    // Port of Go TestEncodeXattrs_CanonicalSorted: keys must sort by their
    // bstr encoding — shorter-length keys first, then bytewise.
    #[test]
    fn encode_xattrs_canonical_sorted() {
        let m = xmap(&[(b"bb", b"2"), (b"a", b"1"), (b"user.selinux", b"x")]);
        let got = encode_xattrs(&m);
        // map(3) | "a"->"1" | "bb"->"2" | "user.selinux"->"x"
        let mut want = vec![0xa3];
        want.extend_from_slice(&bstr(b"a"));
        want.extend_from_slice(&bstr(b"1"));
        want.extend_from_slice(&bstr(b"bb"));
        want.extend_from_slice(&bstr(b"2"));
        want.extend_from_slice(&bstr(b"user.selinux"));
        want.extend_from_slice(&bstr(b"x"));
        assert_eq!(got, want);
    }

    // Encoded-bytes order is length-first: "z" sorts before "ab" even though
    // plain bytewise order would put "ab" first.
    #[test]
    fn encode_xattrs_sorts_by_encoding_not_raw_key() {
        let m = xmap(&[(b"z", b"1"), (b"ab", b"2")]);
        let got = encode_xattrs(&m);
        let mut want = vec![0xa2];
        want.extend_from_slice(&bstr(b"z"));
        want.extend_from_slice(&bstr(b"1"));
        want.extend_from_slice(&bstr(b"ab"));
        want.extend_from_slice(&bstr(b"2"));
        assert_eq!(got, want);
    }

    // Port of Go TestEncodeXattrs_Empty.
    #[test]
    fn encode_xattrs_empty() {
        assert_eq!(encode_xattrs(&BTreeMap::new()), vec![0xa0]);
    }

    #[test]
    fn decode_xattrs_roundtrip() {
        let long_key: Vec<u8> = b"user.a-fairly-long-xattr-name-over-23-bytes".to_vec();
        let mut m = xmap(&[
            (b"a", b"1"),
            (b"bb", &[0u8, 1, 2, 255]),
            (b"user.selinux", b""),
            (b"", b"empty-name"),
        ]);
        m.insert(long_key, vec![9u8; 300]);
        let enc = encode_xattrs(&m);
        assert_eq!(decode_xattrs(&enc).unwrap(), m);
    }

    #[test]
    fn decode_xattrs_rejects_wrong_major() {
        assert_eq!(decode_xattrs(&[0x40]), Err(Error::ExpectedMap { got: 2 }));
        assert_eq!(decode_xattrs(&[0x80]), Err(Error::ExpectedMap { got: 4 }));
        assert_eq!(decode_xattrs(&[]), Err(Error::UnexpectedEof));
    }

    #[test]
    fn decode_xattrs_rejects_trailing_bytes() {
        let mut enc = encode_xattrs(&xmap(&[(b"a", b"1")]));
        enc.extend_from_slice(&[0x00, 0x01]);
        assert_eq!(decode_xattrs(&enc), Err(Error::TrailingBytes(2)));
    }

    #[test]
    fn decode_xattrs_wraps_key_and_value_errors() {
        // Map of one pair whose key claims 2 bytes but only 1 follows: the
        // key read hits EOF. (A bare `[0xa1]` now trips the pair-count bound
        // before the key read.)
        assert_eq!(
            decode_xattrs(&[0xa1, 0x42, 0x61]),
            Err(Error::XattrKey {
                index: 0,
                source: Box::new(Error::UnexpectedEof),
            })
        );
        // Second pair's key is a uint, not a byte string.
        let mut b = vec![0xa2];
        b.extend_from_slice(&bstr(b"a"));
        b.extend_from_slice(&bstr(b"1"));
        b.push(0x05);
        assert_eq!(
            decode_xattrs(&b),
            Err(Error::XattrKey {
                index: 1,
                source: Box::new(Error::ExpectedByteString { got: 0 }),
            })
        );
        // Value for key "a" is a map, not a byte string.
        let mut b = vec![0xa1];
        b.extend_from_slice(&bstr(b"a"));
        b.push(0xa0);
        assert_eq!(
            decode_xattrs(&b),
            Err(Error::XattrValue {
                name: b"a".to_vec(),
                source: Box::new(Error::ExpectedByteString { got: 5 }),
            })
        );
    }

    // Matches Go: readHead's laxness makes DecodeXattrs accept non-shortest
    // length heads inside the map.
    #[test]
    fn decode_xattrs_accepts_non_shortest_heads() {
        let b = [0xa1, 0x58, 0x01, b'a', 0x41, b'1'];
        assert_eq!(decode_xattrs(&b).unwrap(), xmap(&[(b"a", b"1")]));
        // Non-shortest map head too.
        let b = [0xb8, 0x01, 0x41, b'a', 0x41, b'1'];
        assert_eq!(decode_xattrs(&b).unwrap(), xmap(&[(b"a", b"1")]));
    }

    // A tiny input claiming an enormous pair count must fail promptly with a
    // clean error and no huge allocation. Deliberate divergence from Go: the
    // Go original passes the attacker-controlled count to `make(map, n)`,
    // which attempts a terabyte-scale bucket allocation and gets OOM-killed
    // (verified against the reference: `DecodeXattrs([0xBA,0xFF,0xFF,0xFF,
    // 0xFF])` dies). The porting contract forbids reproducing that
    // ("never panic on untrusted input"); see port-notes/cbor.md.
    #[test]
    fn decode_xattrs_huge_count_errors_promptly() {
        for head in [
            &[0xBA, 0xFF, 0xFF, 0xFF, 0xFF][..], // 2^32-1 pairs
            &[0xBB, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF][..], // 2^64-1 pairs
            &[0xBB, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00][..], // 2^40 pairs
        ] {
            match decode_xattrs(head) {
                Err(Error::PairCount { .. }) => {}
                other => panic!("input {head:02x?}: got {other:?}, want PairCount error"),
            }
        }
    }

    // Missing value after a valid key: wrapped as an XattrValue EOF.
    #[test]
    fn decode_xattrs_missing_value() {
        assert_eq!(
            decode_xattrs(&[0xa1, 0x41, b'a']),
            Err(Error::XattrValue {
                name: b"a".to_vec(),
                source: Box::new(Error::UnexpectedEof),
            })
        );
    }

    // Non-UTF-8 key names must format losslessly enough to not panic (Go's
    // %q shows \xNN escapes; the lossy adaptation shows U+FFFD).
    #[test]
    fn error_display_non_utf8_name() {
        let e = Error::XattrValue {
            name: vec![0xff, b'a'],
            source: Box::new(Error::UnexpectedEof),
        };
        assert_eq!(
            e.to_string(),
            "cbor: xattr value for \"\u{fffd}a\": cbor: unexpected EOF"
        );
    }

    // Go map-insert semantics: a duplicate key keeps the last value.
    #[test]
    fn decode_xattrs_duplicate_key_last_wins() {
        let mut b = vec![0xa2];
        b.extend_from_slice(&bstr(b"a"));
        b.extend_from_slice(&bstr(b"1"));
        b.extend_from_slice(&bstr(b"a"));
        b.extend_from_slice(&bstr(b"2"));
        assert_eq!(decode_xattrs(&b).unwrap(), xmap(&[(b"a", b"2")]));
    }

    #[test]
    fn error_display_content() {
        assert_eq!(Error::UnexpectedEof.to_string(), "cbor: unexpected EOF");
        assert_eq!(
            Error::UnsupportedAdditionalInfo(31).to_string(),
            "cbor: unsupported additional info 31"
        );
        assert_eq!(
            Error::ExpectedMap { got: 2 }.to_string(),
            "cbor: expected CBOR map (major 5), got major 2"
        );
        assert_eq!(
            Error::ExpectedByteString { got: 5 }.to_string(),
            "cbor: expected byte string (major 2), got major 5"
        );
        assert_eq!(
            Error::TrailingBytes(3).to_string(),
            "cbor: 3 trailing bytes after xattr map"
        );
        assert_eq!(
            Error::XattrKey {
                index: 1,
                source: Box::new(Error::UnexpectedEof),
            }
            .to_string(),
            "cbor: xattr key 1: cbor: unexpected EOF"
        );
        assert_eq!(
            Error::XattrValue {
                name: b"user.x".to_vec(),
                source: Box::new(Error::ExpectedByteString { got: 0 }),
            }
            .to_string(),
            "cbor: xattr value for \"user.x\": cbor: expected byte string (major 2), got major 0"
        );
        // Wrapped errors expose their source.
        let e = Error::XattrKey {
            index: 0,
            source: Box::new(Error::UnexpectedEof),
        };
        let src = std::error::Error::source(&e).unwrap();
        assert_eq!(src.to_string(), "cbor: unexpected EOF");
    }
}
