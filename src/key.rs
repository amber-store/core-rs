//! The 32-byte Amber-Store lookup key: a content address that encodes a CAS
//! object type, a logical payload length, and a truncated BLAKE3 hash of the
//! payload's serialized bytes. See `architecture/keys.md`.

use std::fmt;

/// Size is the fixed byte length of every key.
pub const SIZE: usize = 32;

/// The 4-bit CAS object type carried in the high nibble of a key's header byte
/// (`architecture/types.md`).
///
/// Unlike the Go implementation (where `Type` is a plain `uint8` that may hold
/// reserved values), this enum can only represent the six defined types.
/// Reserved raw values (6..=15, and anything above the 4-bit field) are handled
/// as `u8` via [`Type::from_u8`] / [`Type::is_valid`] and surface as
/// [`Error::ReservedType`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Type {
    /// Raw file-content byte chunk (a CDC leaf).
    Blob = 0,
    /// File chunk-index node.
    FileNode = 1,
    /// A contiguous run of directory entries.
    DirLeaf = 2,
    /// Directory index node.
    DirNode = 3,
    /// Spilled extended attributes.
    XattrSet = 4,
    /// Snapshot record: tree, parent commits, author, committer, message
    /// (`crate::commit`).
    Commit = 5,
}

impl Type {
    /// Reports whether `v` is a defined CAS object type (0..5). Values 6..15
    /// are reserved and must not be emitted; values above 15 do not fit the
    /// 4-bit field.
    pub const fn is_valid(v: u8) -> bool {
        v <= Type::Commit as u8
    }

    /// Converts a raw type value to a [`Type`], or `None` if `v` is reserved
    /// (6..15) or out of range.
    pub const fn from_u8(v: u8) -> Option<Type> {
        match v {
            0 => Some(Type::Blob),
            1 => Some(Type::FileNode),
            2 => Some(Type::DirLeaf),
            3 => Some(Type::DirNode),
            4 => Some(Type::XattrSet),
            5 => Some(Type::Commit),
            _ => None,
        }
    }
}

/// The type name (`"Blob"`, `"FileNode"`, ...), mirroring Go's
/// `Type.String()`. Go's `"Type(n)"` fallback for reserved values is
/// unrepresentable here; reserved raw values are reported numerically by
/// [`Error::ReservedType`] instead.
impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Type::Blob => "Blob",
            Type::FileNode => "FileNode",
            Type::DirLeaf => "DirLeaf",
            Type::DirNode => "DirNode",
            Type::XattrSet => "XattrSet",
            Type::Commit => "Commit",
        })
    }
}

/// Errors returned by [`Key::parse`] and [`Key::validate`], mirroring the Go
/// package's sentinel errors. Match on the variants (the enum derives
/// `PartialEq`) where Go code would use `errors.Is`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The input to [`Key::parse`] was not exactly [`SIZE`] bytes; carries the
    /// actual length.
    #[error("key: data is not 32 bytes: got {0}")]
    BadKeyLength(usize),
    /// The header's reserved bit (bit 3) is set.
    #[error("key: reserved header bit is set")]
    ReservedBitSet,
    /// The object type is reserved (6..15) or out of range; carries the raw
    /// type value.
    #[error("key: reserved object type: {0}")]
    ReservedType(u8),
    /// The length field has leading-zero padding.
    #[error("key: non-canonical length encoding")]
    NonCanonicalLength,
}

/// A 32-byte lookup key. It is a small `Copy` value type, directly comparable
/// (`Eq`/`Ord`/`Hash`, byte-lexicographic order), so it can be used as a map
/// key. Accessors assume the key is canonical (produced by [`Key::new`],
/// [`Key::new_from_hash`], or [`Key::parse`]).
///
/// The inner byte array is public, mirroring Go's transparent `[32]byte`;
/// bytes obtained from untrusted input must go through [`Key::parse`] (or
/// [`Key::validate`]) before the accessors are used.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(pub [u8; SIZE]);

/// Returns the minimum number of bytes needed to hold `length` big-endian with
/// no leading zero. Zero is the special case: a single `0x00` byte.
fn length_size_for(length: u64) -> usize {
    if length == 0 {
        return 1;
    }
    (u64::BITS - length.leading_zeros()).div_ceil(8) as usize
}

impl Key {
    /// Computes the BLAKE3-256 digest of `serialized`, then assembles a
    /// canonical key via [`Key::new_from_hash`]. `length` is the logical
    /// payload length and is taken as given (it need not equal
    /// `serialized.len()` — see [`Key::new_from_hash`]).
    pub fn new(t: Type, length: u64, serialized: &[u8]) -> Key {
        Key::new_from_hash(t, length, *blake3::hash(serialized).as_bytes())
    }

    /// Assembles a canonical key from a CAS object type, a logical payload
    /// length, and a precomputed full 256-bit BLAKE3 digest. The digest is
    /// truncated to its leading bytes to fill the key. `length` is used
    /// verbatim: for `Blob`/`XattrSet` it is the serialized byte length; for
    /// `FileNode`/`DirLeaf`/`DirNode` and `Commit` it is a logical size (see
    /// `architecture/types.md`).
    ///
    /// Infallible, unlike Go's `NewFromHash`: its only error
    /// (`ErrReservedType`) is unrepresentable because [`Type`] admits only
    /// defined types.
    pub fn new_from_hash(t: Type, length: u64, full_hash: [u8; SIZE]) -> Key {
        let ls = length_size_for(length);
        let mut k = [0u8; SIZE];
        k[0] = (t as u8) << 4 | (ls as u8 - 1);
        let buf = length.to_be_bytes();
        k[1..1 + ls].copy_from_slice(&buf[8 - ls..]);
        k[1 + ls..].copy_from_slice(&full_hash[..SIZE - 1 - ls]);
        Key(k)
    }

    /// Copies `b` into a `Key` and validates its canonical form. `b` must be
    /// exactly [`SIZE`] bytes.
    pub fn parse(b: &[u8]) -> Result<Key, Error> {
        if b.len() != SIZE {
            return Err(Error::BadKeyLength(b.len()));
        }
        let mut k = [0u8; SIZE];
        k.copy_from_slice(b);
        let k = Key(k);
        k.validate()?;
        Ok(k)
    }

    /// Reports whether the key is canonical: the reserved bit is clear, the
    /// type is defined (0..5), and the length field is minimally encoded (its
    /// first byte is non-zero, except for the single `0x00` byte that encodes
    /// a zero length).
    pub fn validate(&self) -> Result<(), Error> {
        if self.0[0] & 0x08 != 0 {
            return Err(Error::ReservedBitSet);
        }
        let raw_type = self.0[0] >> 4;
        if !Type::is_valid(raw_type) {
            return Err(Error::ReservedType(raw_type));
        }
        if self.0[1] == 0 && !(self.length_size() == 1 && self.length() == 0) {
            return Err(Error::NonCanonicalLength);
        }
        Ok(())
    }

    /// Returns the CAS object type from the header's high nibble.
    ///
    /// # Panics
    ///
    /// Panics if the type nibble is reserved, which cannot happen on a
    /// canonical key: untrusted bytes must go through [`Key::parse`] or
    /// [`Key::validate`] first, so reaching the panic is a caller bug (raw
    /// construction without validation).
    pub fn type_(&self) -> Type {
        let raw = self.0[0] >> 4;
        match Type::from_u8(raw) {
            Some(t) => t,
            None => panic!("key: accessor on non-canonical key: reserved object type {raw}"),
        }
    }

    /// Returns the number of bytes the payload-length field occupies (1..8).
    pub fn length_size(&self) -> usize {
        (self.0[0] & 0x07) as usize + 1
    }

    /// Decodes the big-endian payload-length field.
    pub fn length(&self) -> u64 {
        let ls = self.length_size();
        let mut buf = [0u8; 8];
        buf[8 - ls..].copy_from_slice(&self.0[1..1 + ls]);
        u64::from_be_bytes(buf)
    }

    /// Returns the truncated payload hash bytes
    /// (`len == SIZE - 1 - length_size()`).
    pub fn hash(&self) -> &[u8] {
        &self.0[1 + self.length_size()..]
    }

    /// The key's raw bytes (Go: `k[:]`).
    pub fn as_bytes(&self) -> &[u8; SIZE] {
        &self.0
    }
}

impl AsRef<[u8]> for Key {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// The lowercase hex encoding of the key, for logs and errors.
impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Key({self})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn hex_of(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn accessors_single_byte_length() {
        // Blob, length 255 (length_size 1), 30-byte hash.
        let mut k = Key([0u8; SIZE]);
        k.0[0] = 0x00; // type 0, reserved 0, length_size-1 = 0
        k.0[1] = 0xFF; // length = 255
        for i in 2..SIZE {
            k.0[i] = i as u8;
        }
        assert_eq!(k.type_(), Type::Blob);
        assert_eq!(k.length_size(), 1);
        assert_eq!(k.length(), 255);
        assert_eq!(k.hash().len(), 30);
        assert_eq!(k.hash(), &k.0[2..]);
    }

    #[test]
    fn accessors_multi_byte_length() {
        // FileNode, length 65536 (length_size 3): header = (1<<4) | (3-1) = 0x12.
        let mut k = Key([0u8; SIZE]);
        k.0[0] = 0x12;
        (k.0[1], k.0[2], k.0[3]) = (0x01, 0x00, 0x00); // 0x010000 = 65536
        assert_eq!(k.type_(), Type::FileNode);
        assert_eq!(k.length_size(), 3);
        assert_eq!(k.length(), 65536);
        assert_eq!(k.hash().len(), 28);
    }

    #[test]
    fn new_from_hash_round_trip() {
        let mut full = [0u8; SIZE];
        for (i, b) in full.iter_mut().enumerate() {
            *b = i as u8 + 1;
        }
        let k = Key::new_from_hash(Type::DirNode, 1000, full);
        assert_eq!(k.type_(), Type::DirNode);
        assert_eq!(k.length(), 1000);
        assert_eq!(k.length_size(), 2);
        assert_eq!(k.hash(), &full[..SIZE - 1 - 2]);
    }

    #[test]
    fn new_from_hash_length_size_boundaries() {
        let mut full = [0u8; SIZE];
        for (i, b) in full.iter_mut().enumerate() {
            *b = i as u8;
        }
        let cases: &[(u64, usize)] = &[
            (0, 1),
            (1, 1),
            (255, 1),
            (256, 2),
            (65535, 2),
            (65536, 3),
            ((1 << 24) - 1, 3),
            (1 << 24, 4),
            ((1 << 32) - 1, 4),
            (1 << 32, 5),
            (1 << 40, 6),
            (1 << 48, 7),
            ((1 << 56) - 1, 7),
            (1 << 56, 8),
            (u64::MAX, 8),
        ];
        for &(length, want_ls) in cases {
            let k = Key::new_from_hash(Type::Blob, length, full);
            assert_eq!(k.length_size(), want_ls, "length {length}");
            assert_eq!(k.length(), length, "length {length}");
            let want_hash_len = SIZE - 1 - want_ls;
            assert_eq!(k.hash().len(), want_hash_len, "length {length}");
            assert_eq!(k.hash(), &full[..want_hash_len], "length {length}");
        }
    }

    #[test]
    fn new_known_answer_and_truncation() {
        // Official BLAKE3-256 hash of the empty input.
        const WANT_HEX: &str = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";
        let full = *blake3::hash(&[]).as_bytes();
        assert_eq!(hex_of(&full), WANT_HEX);
        // new(Blob, 0, empty): empty blob, length_size 1, hash len 30.
        let k = Key::new(Type::Blob, 0, &[]);
        assert_eq!(k.length(), 0);
        assert_eq!(k.length_size(), 1);
        assert_eq!(k.hash(), &full[..30]);
        // new must equal new_from_hash on the same content's digest.
        let k2 = Key::new_from_hash(Type::Blob, 0, full);
        assert_eq!(k, k2);
    }

    #[test]
    fn key_deterministic_and_comparable() {
        let content = b"amber-store determinism check";
        let a = Key::new(Type::FileNode, content.len() as u64, content);
        let b = Key::new(Type::FileNode, content.len() as u64, content);
        assert_eq!(a, b, "new is not deterministic for identical inputs");
        // Keys must be usable as hash-map keys.
        let mut m = HashMap::new();
        m.insert(a, 1);
        assert_eq!(m.get(&b), Some(&1));
    }

    #[test]
    fn new_length_is_logical_not_serialized() {
        // The length field is the logical payload size and is passed verbatim;
        // it is NOT derived from or validated against serialized.len(). A
        // FileNode covering a 1 MiB file region has length 1<<20 even though
        // its own serialized bytes (here stand-in content) are tiny. The hash
        // still covers the serialized bytes, not the logical length.
        let content = b"tiny";
        let k = Key::new(Type::FileNode, 1 << 20, content);
        assert_eq!(k.length(), 1 << 20);
        let full = blake3::hash(content);
        assert_eq!(k.hash(), &full.as_bytes()[..k.hash().len()]);
    }

    #[test]
    fn parse_round_trip() {
        let mut full = [0u8; SIZE];
        for (i, b) in full.iter_mut().enumerate() {
            *b = i as u8 + 1;
        }
        for t in [
            Type::Blob,
            Type::FileNode,
            Type::DirLeaf,
            Type::DirNode,
            Type::XattrSet,
            Type::Commit,
        ] {
            let k = Key::new_from_hash(t, 12345, full);
            let got = Key::parse(&k.0).unwrap();
            assert_eq!(got, k, "{t}: round-trip mismatch");
        }
    }

    #[test]
    fn parse_bad_length() {
        for n in [0usize, 31, 33, 64] {
            assert_eq!(
                Key::parse(&vec![0u8; n]),
                Err(Error::BadKeyLength(n)),
                "len {n}"
            );
        }
    }

    #[test]
    fn validate_reserved_bit() {
        let mut k = Key::new_from_hash(Type::Blob, 1, [0u8; SIZE]);
        k.0[0] |= 0x08; // set the reserved bit
        assert_eq!(k.validate(), Err(Error::ReservedBitSet));
    }

    #[test]
    fn validate_reserved_type() {
        let mut k = Key([0u8; SIZE]);
        k.0[0] = 6 << 4; // type 6, length_size 1
        k.0[1] = 0x01;
        assert_eq!(k.validate(), Err(Error::ReservedType(6)));
    }

    #[test]
    fn validate_non_canonical_length() {
        // Blob, length_size 2 (header low bits = 1), length bytes 0x00 0x05:
        // leading zero with a non-zero value -> non-canonical.
        let mut k = Key([0u8; SIZE]);
        k.0[0] = 0x01;
        (k.0[1], k.0[2]) = (0x00, 0x05);
        assert_eq!(k.validate(), Err(Error::NonCanonicalLength));
    }

    #[test]
    fn validate_zero_length_is_canonical() {
        // Blob, length_size 1, length byte 0x00: value 0 is the allowed
        // special case.
        let k = Key([0u8; SIZE]); // all zero bytes
        assert_eq!(k.validate(), Ok(()));
    }

    #[test]
    fn validate_order_reserved_bit_before_type() {
        // Both the reserved bit and a reserved type set: Go checks the
        // reserved bit first.
        let mut k = Key([0u8; SIZE]);
        k.0[0] = 5 << 4 | 0x08;
        k.0[1] = 0x01;
        assert_eq!(k.validate(), Err(Error::ReservedBitSet));
    }

    #[test]
    fn display_hex() {
        let mut k = Key([0u8; SIZE]);
        k.0[0] = 0x12;
        k.0[31] = 0xFF;
        let got = k.to_string();
        assert_eq!(got, hex_of(&k.0));
        assert_eq!(got.len(), 2 * SIZE);
    }

    /// Port of Go `TestNewFromHash_Commit`.
    #[test]
    fn new_from_hash_commit() {
        let k = Key::new_from_hash(Type::Commit, 100, [0u8; SIZE]);
        assert_eq!(k.0[0], 0x50, "type 5, one length byte");
        assert_eq!(k.type_(), Type::Commit);
        assert_eq!(k.length(), 100);
        assert_eq!(k.validate(), Ok(()));
    }

    #[test]
    fn type_is_valid() {
        for v in 0u8..=5 {
            assert!(Type::is_valid(v), "Type({v}) should be valid");
        }
        for v in [6u8, 7, 15, 16, 255] {
            assert!(!Type::is_valid(v), "Type({v}) should be invalid");
        }
    }

    #[test]
    fn type_from_u8() {
        assert_eq!(Type::from_u8(0), Some(Type::Blob));
        assert_eq!(Type::from_u8(1), Some(Type::FileNode));
        assert_eq!(Type::from_u8(2), Some(Type::DirLeaf));
        assert_eq!(Type::from_u8(3), Some(Type::DirNode));
        assert_eq!(Type::from_u8(4), Some(Type::XattrSet));
        assert_eq!(Type::from_u8(5), Some(Type::Commit));
        for v in [6u8, 15, 16, 255] {
            assert_eq!(Type::from_u8(v), None, "Type({v})");
        }
    }

    #[test]
    fn type_display() {
        let cases = [
            (Type::Blob, "Blob"),
            (Type::FileNode, "FileNode"),
            (Type::DirLeaf, "DirLeaf"),
            (Type::DirNode, "DirNode"),
            (Type::XattrSet, "XattrSet"),
            (Type::Commit, "Commit"),
        ];
        for (t, want) in cases {
            assert_eq!(t.to_string(), want);
        }
        // Go's `Type(7).String() == "Type(7)"` fallback is unrepresentable on
        // the Rust enum; the numeric form appears in Error::ReservedType.
        assert_eq!(
            Error::ReservedType(7).to_string(),
            "key: reserved object type: 7"
        );
    }

    #[test]
    fn error_messages_match_go() {
        assert_eq!(
            Error::BadKeyLength(5).to_string(),
            "key: data is not 32 bytes: got 5"
        );
        assert_eq!(
            Error::ReservedBitSet.to_string(),
            "key: reserved header bit is set"
        );
        assert_eq!(
            Error::ReservedType(9).to_string(),
            "key: reserved object type: 9"
        );
        assert_eq!(
            Error::NonCanonicalLength.to_string(),
            "key: non-canonical length encoding"
        );
    }

    #[test]
    #[should_panic(expected = "reserved object type")]
    fn type_accessor_panics_on_reserved_nibble() {
        let mut k = Key([0u8; SIZE]);
        k.0[0] = 7 << 4;
        let _ = k.type_();
    }

    #[test]
    fn ord_is_byte_lexicographic() {
        // Downstream modules (packstore index, fstree binary search) require
        // Key ordering to match Go's bytes.Compare on k[:].
        let mut a = Key([0u8; SIZE]);
        let mut b = Key([0u8; SIZE]);
        a.0[0] = 0x01;
        b.0[0] = 0x02;
        assert!(a < b, "first byte dominates");
        let mut c = Key([0xFFu8; SIZE]);
        c.0[0] = 0x01;
        assert!(a < c && c < b, "later bytes break ties only");
        let mut keys = vec![b, c, a];
        keys.sort();
        assert_eq!(keys, vec![a, c, b]);
        let mut raw: Vec<[u8; SIZE]> = vec![b.0, c.0, a.0];
        raw.sort();
        assert_eq!(
            keys.iter().map(|k| k.0).collect::<Vec<_>>(),
            raw,
            "Key order must equal raw byte order"
        );
    }

    #[test]
    fn debug_and_byte_accessors() {
        let mut k = Key([0u8; SIZE]);
        k.0[0] = 0x12;
        k.0[31] = 0xFF;
        assert_eq!(format!("{k:?}"), format!("Key({k})"));
        assert_eq!(k.as_bytes(), &k.0);
        assert_eq!(k.as_ref(), &k.0[..]);
    }
}
