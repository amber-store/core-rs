//! The 32-byte Amber-Store lookup key: a content address that encodes a CAS
//! object type, a logical payload length, and a truncated BLAKE3 hash of the
//! payload's serialized bytes. See `architecture/keys.md`.

use std::fmt;

/// Size is the fixed byte length of every key.
pub const SIZE: usize = 32;

/// The byte length of the SHA-1 a [`Type::GitSha1`] key holds.
pub const GIT_SHA1_SIZE: usize = 20;

/// The 4-bit CAS object type carried in the high nibble of a key's header byte
/// (`architecture/types.md`).
///
/// Unlike the Go implementation (where `Type` is a plain `uint8` that may hold
/// reserved values), this enum can only represent the seven defined types.
/// Reserved raw values (7..=15, and anything above the 4-bit field) are handled
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
    /// A git object, addressed by its SHA-1 (`crate::gitobj`,
    /// `architecture/git-objects.md`).
    GitSha1 = 6,
}

impl Type {
    /// Reports whether `v` is a defined CAS object type (0..6). Values 7..15
    /// are reserved and must not be emitted; values above 15 do not fit the
    /// 4-bit field.
    pub const fn is_valid(v: u8) -> bool {
        v <= Type::GitSha1 as u8
    }

    /// Converts a raw type value to a [`Type`], or `None` if `v` is reserved
    /// (7..15) or out of range.
    pub const fn from_u8(v: u8) -> Option<Type> {
        match v {
            0 => Some(Type::Blob),
            1 => Some(Type::FileNode),
            2 => Some(Type::DirLeaf),
            3 => Some(Type::DirNode),
            4 => Some(Type::XattrSet),
            5 => Some(Type::Commit),
            6 => Some(Type::GitSha1),
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
            Type::GitSha1 => "GitSHA1",
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
    /// The object type is reserved (7..15) or out of range; carries the raw
    /// type value.
    #[error("key: reserved object type: {0}")]
    ReservedType(u8),
    /// The length field has leading-zero padding.
    #[error("key: non-canonical length encoding")]
    NonCanonicalLength,
    /// A [`Type::GitSha1`] key has something other than zeros after its SHA-1.
    #[error("key: non-canonical git key")]
    NonCanonicalGitKey,
}

/// A 32-byte lookup key. It is a small `Copy` value type, directly comparable
/// (`Eq`/`Ord`/`Hash`, byte-lexicographic order), so it can be used as a map
/// key. Accessors assume the key is canonical (produced by [`Key::new`],
/// [`Key::new_from_hash`], or [`Key::parse`]).
///
/// The bytes are the (header, length, hash) encoding of
/// `architecture/keys.md` reversed: the truncated hash comes first and the
/// header byte last, so keys sort and bucket uniformly on their leading
/// bytes. A [`Type::GitSha1`] key has a layout of its own, see
/// [`Key::new_git_sha1`].
///
/// The inner byte array is public, mirroring Go's transparent `[32]byte`;
/// bytes obtained from untrusted input must go through [`Key::parse`] (or
/// [`Key::validate`]) before the accessors are used.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(pub [u8; SIZE]);

/// The index of the header byte: the last one (Go: `headerAt`).
const HEADER_AT: usize = SIZE - 1;

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
    /// `serialized.len()` — see [`Key::new_from_hash`]). A [`Type::GitSha1`]
    /// key is not made here: see [`Key::new_git_sha1`].
    ///
    /// # Panics
    ///
    /// Panics on [`Type::GitSha1`], as [`Key::new_from_hash`] does.
    pub fn new(t: Type, length: u64, serialized: &[u8]) -> Key {
        Key::new_from_hash(t, length, *blake3::hash(serialized).as_bytes())
    }

    /// Assembles a canonical key from a CAS object type, a logical payload
    /// length, and a precomputed full 256-bit BLAKE3 digest. The digest is
    /// truncated to its leading bytes to fill the key, and the whole encoding
    /// is byte-reversed (see [`Key`]). `length` is used verbatim: for `Blob`/`XattrSet` it is the serialized byte length; for
    /// `FileNode`/`DirLeaf`/`DirNode` and `Commit` it is a logical size (see
    /// `architecture/types.md`).
    ///
    /// Infallible, unlike Go's `NewFromHash`: `ErrReservedType` is
    /// unrepresentable because [`Type`] admits only defined types.
    ///
    /// # Panics
    ///
    /// Panics on [`Type::GitSha1`], where Go returns `ErrNotBlake3`: such a key
    /// would look canonical and name no git object at all. Asking for one is a
    /// caller bug, and [`Key::new_git_sha1`] makes the real thing.
    pub fn new_from_hash(t: Type, length: u64, full_hash: [u8; SIZE]) -> Key {
        assert!(
            t != Type::GitSha1,
            "key: this type is not addressed by BLAKE3: {t}"
        );
        let ls = length_size_for(length);
        let mut k = [0u8; SIZE];
        k[HEADER_AT] = (t as u8) << 4 | (ls as u8 - 1);
        k[HEADER_AT - ls..HEADER_AT].copy_from_slice(&length.to_le_bytes()[..ls]);
        let hash = &mut k[..HEADER_AT - ls];
        hash.copy_from_slice(&full_hash[..HEADER_AT - ls]);
        hash.reverse();
        Key(k)
    }

    /// Assembles the key of a git object from its SHA-1. The layout is not the
    /// reversed one: the SHA-1 comes first, in digest order, so a key's hex
    /// starts with the object's git id and keys sort as git ids do. Then come
    /// ten zero bytes, the length (a git object's id does not tell its size, so
    /// a key made from a link in another object could not hold one: it is
    /// always zero) and the header. See `architecture/keys.md`.
    pub fn new_git_sha1(sum: [u8; GIT_SHA1_SIZE]) -> Key {
        let mut k = [0u8; SIZE];
        k[..GIT_SHA1_SIZE].copy_from_slice(&sum);
        k[HEADER_AT] = (Type::GitSha1 as u8) << 4;
        Key(k)
    }

    /// The SHA-1 a [`Type::GitSha1`] key holds, or `None` for a key of any other
    /// type (Go: `GitSHA1Sum`).
    pub fn git_sha1_sum(&self) -> Option<[u8; GIT_SHA1_SIZE]> {
        if !self.is_git() {
            return None;
        }
        let mut sum = [0u8; GIT_SHA1_SIZE];
        sum.copy_from_slice(&self.0[..GIT_SHA1_SIZE]);
        Some(sum)
    }

    /// Whether the type nibble says [`Type::GitSha1`], on any bytes.
    fn is_git(&self) -> bool {
        self.0[HEADER_AT] >> 4 == Type::GitSha1 as u8
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
    /// type is defined (0..6), the length field is minimally encoded (its most
    /// significant byte, the one next to the header, is non-zero, except for
    /// the single `0x00` byte that encodes a zero length), and a
    /// [`Type::GitSha1`] key has nothing but zeros after its SHA-1, so that one
    /// git object has one key.
    pub fn validate(&self) -> Result<(), Error> {
        if self.0[HEADER_AT] & 0x08 != 0 {
            return Err(Error::ReservedBitSet);
        }
        let raw_type = self.0[HEADER_AT] >> 4;
        if !Type::is_valid(raw_type) {
            return Err(Error::ReservedType(raw_type));
        }
        if self.0[HEADER_AT - 1] == 0 && !(self.length_size() == 1 && self.length() == 0) {
            return Err(Error::NonCanonicalLength);
        }
        if self.is_git() && !self.has_git_tail() {
            return Err(Error::NonCanonicalGitKey);
        }
        Ok(())
    }

    /// Whether everything between a git key's SHA-1 and its type is zero: the
    /// padding, the length and the length size.
    fn has_git_tail(&self) -> bool {
        self.0[HEADER_AT] & 0x07 == 0 && self.0[GIT_SHA1_SIZE..HEADER_AT].iter().all(|&b| b == 0)
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
        let raw = self.0[HEADER_AT] >> 4;
        match Type::from_u8(raw) {
            Some(t) => t,
            None => panic!("key: accessor on non-canonical key: reserved object type {raw}"),
        }
    }

    /// Returns the number of bytes the payload-length field occupies (1..8).
    pub fn length_size(&self) -> usize {
        (self.0[HEADER_AT] & 0x07) as usize + 1
    }

    /// Decodes the payload-length field, which sits just before the header
    /// byte, least significant byte first.
    pub fn length(&self) -> u64 {
        let ls = self.length_size();
        let mut buf = [0u8; 8];
        buf[..ls].copy_from_slice(&self.0[HEADER_AT - ls..HEADER_AT]);
        u64::from_le_bytes(buf)
    }

    /// Returns the payload hash in digest order, a copy (Go: `Hash`, likewise a
    /// copy). For a BLAKE3 key that is the truncated digest
    /// (`len == SIZE - 1 - length_size()`), which the key stores reversed; for a
    /// [`Type::GitSha1`] key it is the SHA-1, [`GIT_SHA1_SIZE`] bytes.
    pub fn hash(&self) -> Vec<u8> {
        if self.is_git() {
            return self.0[..GIT_SHA1_SIZE].to_vec();
        }
        let mut h = self.0[..HEADER_AT - self.length_size()].to_vec();
        h.reverse();
        h
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
        k.0[31] = 0x00; // type 0, reserved 0, length_size-1 = 0
        k.0[30] = 0xFF; // length = 255
        for i in 0..30 {
            k.0[i] = i as u8;
        }
        assert_eq!(k.type_(), Type::Blob);
        assert_eq!(k.length_size(), 1);
        assert_eq!(k.length(), 255);
        assert_eq!(k.hash().len(), 30);
        // The hash is stored reversed; hash() hands it back in digest order.
        let mut want = k.0[..30].to_vec();
        want.reverse();
        assert_eq!(k.hash(), want);
    }

    #[test]
    fn accessors_multi_byte_length() {
        // FileNode, length 65536 (length_size 3): header = (1<<4) | (3-1) = 0x12.
        let mut k = Key([0u8; SIZE]);
        k.0[31] = 0x12;
        (k.0[30], k.0[29], k.0[28]) = (0x01, 0x00, 0x00); // 0x010000 = 65536, low byte first
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

    /// Port of Go `TestNewFromHash_Layout`.
    #[test]
    fn new_from_hash_layout() {
        // The key is the (header, big-endian length, hash prefix) encoding
        // with its 32 bytes reversed: hash first, header last.
        let mut full = [0u8; SIZE];
        for (i, b) in full.iter_mut().enumerate() {
            *b = i as u8 + 1;
        }
        let k = Key::new_from_hash(Type::DirNode, 1000, full);
        // DirNode, two length bytes, 1000.
        let mut unflipped = vec![0x31, 0x03, 0xE8];
        unflipped.extend_from_slice(&full[..29]);
        for (i, b) in unflipped.iter().enumerate() {
            assert_eq!(k.0[SIZE - 1 - i], *b, "k[{}] (key {k})", SIZE - 1 - i);
        }
    }

    /// Port of Go `TestNewFromHash_LowEntropyFieldsAtTheEnd`.
    #[test]
    fn new_from_hash_low_entropy_fields_at_the_end() {
        // Keys of one type and length differ only in their hash, so they
        // share their tail (length, header) and spread uniformly on their
        // first byte.
        const N: usize = 4096;
        let mut first = std::collections::HashSet::new();
        let tail = Key::new(Type::Blob, 200, &[0, 0]).0[SIZE - 2..].to_vec();
        for i in 0..N {
            let k = Key::new(Type::Blob, 200, &[i as u8, (i >> 8) as u8]);
            first.insert(k.0[0]);
            assert_eq!(&k.0[SIZE - 2..], &tail[..], "key {i}");
        }
        assert_eq!(
            first.len(),
            256,
            "{N} keys must cover every first-byte value"
        );
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
        // The whole key: the 30-byte hash prefix reversed, the length, the
        // header.
        assert_eq!(
            k.to_string(),
            "1fe4ca939accb712c1adc925cb9b49c9dc36ea4d40a0a6a1f9f5b94913af0000"
        );
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
        k.0[31] |= 0x08; // set the reserved bit
        assert_eq!(k.validate(), Err(Error::ReservedBitSet));
    }

    #[test]
    fn validate_reserved_type() {
        let mut k = Key([0u8; SIZE]);
        k.0[31] = 7 << 4; // type 7, length_size 1
        k.0[30] = 0x01;
        assert_eq!(k.validate(), Err(Error::ReservedType(7)));
    }

    #[test]
    fn validate_non_canonical_length() {
        // Blob, length_size 2 (header low bits = 1), length bytes 0x00 0x05:
        // leading zero with a non-zero value -> non-canonical.
        let mut k = Key([0u8; SIZE]);
        k.0[31] = 0x01;
        (k.0[30], k.0[29]) = (0x00, 0x05);
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
        k.0[31] = 5 << 4 | 0x08;
        k.0[30] = 0x01;
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
        assert_eq!(k.0[31], 0x50, "type 5, one length byte");
        assert_eq!(k.type_(), Type::Commit);
        assert_eq!(k.length(), 100);
        assert_eq!(k.validate(), Ok(()));
    }

    // The object id git gives an empty blob, and the key that stands for it
    // (Go: `key/git_test.go`).
    const EMPTY_BLOB_OID: &str = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";

    fn empty_blob_sum() -> [u8; GIT_SHA1_SIZE] {
        let mut sum = [0u8; GIT_SHA1_SIZE];
        for (i, b) in sum.iter_mut().enumerate() {
            *b = u8::from_str_radix(&EMPTY_BLOB_OID[2 * i..2 * i + 2], 16).unwrap();
        }
        sum
    }

    #[test]
    fn git_key_layout() {
        let k = Key::new_git_sha1(empty_blob_sum());
        let want = format!("{EMPTY_BLOB_OID}{}0060", "00".repeat(10));
        assert_eq!(k.to_string(), want);
        assert_eq!(k.validate(), Ok(()));
    }

    #[test]
    fn git_key_accessors() {
        let sum = empty_blob_sum();
        let k = Key::new_git_sha1(sum);
        assert_eq!(k.type_(), Type::GitSha1);
        assert_eq!((k.length(), k.length_size()), (0, 1));
        // A git key's hash is the SHA-1 alone, in digest order: not the 30
        // bytes a BLAKE3 key's hash takes, and not reversed.
        assert_eq!(k.hash(), sum.to_vec());
        assert_eq!(k.git_sha1_sum(), Some(sum));
        assert_eq!(Key::new(Type::Blob, 0, &[]).git_sha1_sum(), None);
    }

    #[test]
    fn git_key_parse_round_trip() {
        let k = Key::new_git_sha1(empty_blob_sum());
        assert_eq!(Key::parse(&k.0), Ok(k));
    }

    /// Only one key may stand for one SHA-1: anything but the zero padding and
    /// the zero length would let the same git object sit under many keys.
    #[test]
    fn git_key_validate_refuses_everything_but_the_layout() {
        let good = Key::new_git_sha1(empty_blob_sum());
        type Edit = fn(&mut Key);
        let cases: &[(&str, Edit, Error)] = &[
            (
                "padding, first byte",
                |k| k.0[GIT_SHA1_SIZE] = 1,
                Error::NonCanonicalGitKey,
            ),
            (
                "padding, last byte",
                |k| k.0[HEADER_AT - 2] = 1,
                Error::NonCanonicalGitKey,
            ),
            (
                "a length",
                |k| k.0[HEADER_AT - 1] = 5,
                Error::NonCanonicalGitKey,
            ),
            (
                "two length bytes, a value",
                |k| {
                    k.0[HEADER_AT] |= 1;
                    k.0[HEADER_AT - 1] = 5;
                },
                Error::NonCanonicalGitKey,
            ),
            (
                "two length bytes, zero",
                |k| k.0[HEADER_AT] |= 1,
                Error::NonCanonicalLength,
            ),
            (
                "reserved bit",
                |k| k.0[HEADER_AT] |= 0x08,
                Error::ReservedBitSet,
            ),
        ];
        for (name, edit, want) in cases {
            let mut k = good;
            edit(&mut k);
            assert_eq!(k.validate(), Err(*want), "{name}");
            assert_eq!(Key::parse(&k.0), Err(*want), "{name}");
        }
    }

    /// The padding rule is the git type's alone: a BLAKE3 key has hash bytes
    /// there.
    #[test]
    fn git_key_rule_leaves_other_types_alone() {
        for t in [
            Type::Blob,
            Type::FileNode,
            Type::DirLeaf,
            Type::DirNode,
            Type::XattrSet,
            Type::Commit,
        ] {
            assert_eq!(Key::new_from_hash(t, 0, [0xab; SIZE]).validate(), Ok(()));
        }
    }

    /// Go: `TestGitKey_NotFromTheBlake3Constructors`, whose `ErrNotBlake3` is a
    /// panic here, these constructors being infallible.
    #[test]
    #[should_panic(expected = "this type is not addressed by BLAKE3: GitSHA1")]
    fn git_key_not_from_new_from_hash() {
        let _ = Key::new_from_hash(Type::GitSha1, 0, [0; SIZE]);
    }

    #[test]
    #[should_panic(expected = "this type is not addressed by BLAKE3: GitSHA1")]
    fn git_key_not_from_new() {
        let _ = Key::new(Type::GitSha1, 0, b"blob 0\0");
    }

    #[test]
    fn git_key_error_text_matches_go() {
        assert_eq!(
            Error::NonCanonicalGitKey.to_string(),
            "key: non-canonical git key"
        );
    }

    #[test]
    fn type_is_valid() {
        for v in 0u8..=6 {
            assert!(Type::is_valid(v), "Type({v}) should be valid");
        }
        for v in [7u8, 8, 15, 16, 255] {
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
        assert_eq!(Type::from_u8(6), Some(Type::GitSha1));
        for v in [7u8, 15, 16, 255] {
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
            (Type::GitSha1, "GitSHA1"),
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
        k.0[31] = 7 << 4;
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
