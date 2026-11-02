//! Git objects as the store holds them: the exact bytes git takes the SHA-1 of,
//! `<kind> <size>\0<content>`, under a [`Type::GitSha1`](crate::key::Type) key.
//! This knows the framing, the hash, and the links one object holds to others,
//! which is all the store needs: it does not check an object the way `git fsck`
//! does. See `architecture/git-objects.md` (Go: package `gitobj`).

use std::fmt;

use sha1_checked::Sha1;

use crate::key::{GIT_SHA1_SIZE, Key};

/// Errors from this module. The text matches Go's, byte for byte.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// A payload is not a git object the store can walk (Go: `ErrMalformed`).
    #[error("gitobj: malformed object: {0}")]
    Malformed(String),
    /// A payload does not hash to the key it was given under (Go: `ErrMismatch`).
    #[error("gitobj: payload does not match its key: payload hashes to {want}, not {got}")]
    Mismatch {
        /// What the payload hashes to.
        want: Key,
        /// The key it was given under.
        got: Key,
    },
    /// A payload carries the marks of a SHA-1 collision attack. Its SHA-1 says
    /// nothing about it, so it must not be stored under one (Go: `ErrCollision`).
    #[error("gitobj: SHA-1 collision attack detected")]
    Collision,
}

fn malformed(msg: impl Into<String>) -> Error {
    Error::Malformed(msg.into())
}

/// What git calls an object's type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// File content.
    Blob,
    /// A directory.
    Tree,
    /// A snapshot and its parents.
    Commit,
    /// An annotated tag.
    Tag,
}

impl Kind {
    /// The name git frames an object with.
    pub const fn name(self) -> &'static str {
        match self {
            Kind::Blob => "blob",
            Kind::Tree => "tree",
            Kind::Commit => "commit",
            Kind::Tag => "tag",
        }
    }

    /// The inverse of [`Kind::name`] for the four kinds there are.
    fn of(name: &[u8]) -> Option<Kind> {
        match name {
            b"blob" => Some(Kind::Blob),
            b"tree" => Some(Kind::Tree),
            b"commit" => Some(Kind::Commit),
            b"tag" => Some(Kind::Tag),
            _ => None,
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Frames `content` as an object of the given kind: the bytes a store holds, and
/// the bytes git takes the SHA-1 of.
pub fn encode(kind: Kind, content: &[u8]) -> Vec<u8> {
    let name = kind.name();
    let size = content.len().to_string();
    let mut out = Vec::with_capacity(name.len() + 1 + size.len() + 1 + content.len());
    out.extend_from_slice(name.as_bytes());
    out.push(b' ');
    out.extend_from_slice(size.as_bytes());
    out.push(0);
    out.extend_from_slice(content);
    out
}

/// Splits a payload into its kind and content, holding the frame to what git
/// reads: a known kind, one space, a decimal size without leading zeros, a NUL,
/// and exactly that many bytes. The content is a part of `data`.
pub fn parse(data: &[u8]) -> Result<(Kind, &[u8]), Error> {
    let Some(sp) = data.iter().position(|&b| b == b' ') else {
        return Err(malformed("no space after the kind"));
    };
    let Some(kind) = Kind::of(&data[..sp]) else {
        return Err(malformed("unknown kind"));
    };
    let rest = &data[sp + 1..];
    let Some(nul) = rest.iter().position(|&b| b == 0) else {
        return Err(malformed("no NUL after the size"));
    };
    let Some(size) = parse_size(&rest[..nul]) else {
        return Err(malformed("bad size"));
    };
    let content = &rest[nul + 1..];
    if size != content.len() as u64 {
        return Err(malformed(format!(
            "size {size}, content of {} bytes",
            content.len()
        )));
    }
    Ok((kind, content))
}

/// Reads a decimal number with no leading zero and no sign.
fn parse_size(b: &[u8]) -> Option<u64> {
    if b.is_empty() || (b[0] == b'0' && b.len() > 1) {
        return None;
    }
    let mut n = 0u64;
    for &c in b {
        if !c.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add(u64::from(c - b'0'))?;
    }
    Some(n)
}

/// The key of a payload: its SHA-1, as [`Key::new_git_sha1`] lays it out. It does
/// not look at the frame. A payload that carries the marks of a collision attack
/// has no key, and gets [`Error::Collision`].
pub fn key_of(data: &[u8]) -> Result<Key, Error> {
    let result = Sha1::try_digest(data);
    if result.has_collision() {
        return Err(Error::Collision);
    }
    let mut sum = [0u8; GIT_SHA1_SIZE];
    sum.copy_from_slice(result.hash());
    Ok(Key::new_git_sha1(sum))
}

/// Reports whether `data` is the object `k` names: not a collision, hashing to
/// `k`, and framed as a git object. It leaves the content alone. git does not parse
/// an object it only reaches as a file, and neither does the store: an object that
/// will not parse is refused by the walk that has to read it ([`links`]), which
/// comes before a reference to it is accepted, and by nothing short of that.
pub fn verify(k: Key, data: &[u8]) -> Result<(), Error> {
    let want = key_of(data)?;
    if want != k {
        return Err(Error::Mismatch { want, got: k });
    }
    parse(data).map(|_| ())
}

/// One object's reference to another. `leaf` says what the referring object knows
/// of the target: it is a blob, which has no links, so a walk may count it without
/// reading it. Git's own walks trust an entry's mode the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Link {
    /// The target.
    pub key: Key,
    /// The target has no links of its own, a walk need not read it.
    pub leaf: bool,
}

/// The objects a payload refers to, in the order it names them: a commit's tree
/// and then its parents, a tree's entries, a tag's target. A blob has none. A tree
/// entry that holds a submodule's commit is no link, since that commit lives in
/// another repository; git does not need it here either.
pub fn links(data: &[u8]) -> Result<Vec<Link>, Error> {
    let (kind, content) = parse(data)?;
    match kind {
        Kind::Blob => Ok(Vec::new()),
        Kind::Tree => tree_links(content),
        Kind::Commit => commit_links(content),
        Kind::Tag => tag_links(content),
    }
}

const HEX_LEN: usize = 2 * GIT_SHA1_SIZE;

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Reads `<prefix><40 hex digits>\n` from the start of `b` and returns the key it
/// names and what follows. `prefix` ends in a space. Hex digits of either case are
/// read, as git does.
fn id_line<'a>(b: &'a [u8], prefix: &str) -> Result<(Key, &'a [u8]), Error> {
    let n = prefix.len();
    let word = &prefix[..n - 1];
    let Some(rest) = b.strip_prefix(prefix.as_bytes()) else {
        return Err(malformed(format!("no \"{word}\" line")));
    };
    let bad = || malformed(format!("bad \"{word}\" line"));
    if rest.len() < HEX_LEN + 1 || rest[HEX_LEN] != b'\n' {
        return Err(bad());
    }
    let mut sum = [0u8; GIT_SHA1_SIZE];
    for (i, byte) in sum.iter_mut().enumerate() {
        let hi = hex_digit(rest[2 * i]).ok_or_else(bad)?;
        let lo = hex_digit(rest[2 * i + 1]).ok_or_else(bad)?;
        *byte = hi << 4 | lo;
    }
    Ok((Key::new_git_sha1(sum), &rest[HEX_LEN + 1..]))
}

/// Reads the lines git reads a commit's links from: a tree line first, the parent
/// lines right after it. What comes later is not looked at: a merge tag or a
/// message may well say "parent". Neither is a `parent ` too short to hold an id,
/// which git does not take for a parent line.
fn commit_links(b: &[u8]) -> Result<Vec<Link>, Error> {
    let (tree, mut rest) = id_line(b, "tree ")?;
    let mut out = vec![Link {
        key: tree,
        leaf: false,
    }];
    while rest.len() > "parent ".len() + HEX_LEN && rest.starts_with(b"parent ") {
        let (parent, after) = id_line(rest, "parent ")?;
        out.push(Link {
            key: parent,
            leaf: false,
        });
        rest = after;
    }
    Ok(out)
}

/// Reads a tag's target from its first two lines. git compares the type as a C
/// string, so it ends at a NUL if there is one.
fn tag_links(b: &[u8]) -> Result<Vec<Link>, Error> {
    let (target, rest) = id_line(b, "object ")?;
    let Some(rest) = rest.strip_prefix(b"type ") else {
        return Err(malformed("no \"type\" line"));
    };
    let bad = || malformed("bad \"type\" line");
    let nl = rest.iter().position(|&c| c == b'\n').ok_or_else(bad)?;
    let name = rest[..nl].split(|&c| c == 0).next().unwrap_or_default();
    let kind = Kind::of(name).ok_or_else(bad)?;
    Ok(vec![Link {
        key: target,
        leaf: kind == Kind::Blob,
    }])
}

// The file-type bits of a tree entry's mode.
const MODE_TYPE_MASK: u32 = 0o170000;
const MODE_DIR: u32 = 0o040000;
const MODE_REGULAR: u32 = 0o100000;
const MODE_SYMLINK: u32 = 0o120000;

/// Reads a tree's entries, each `<octal mode> <name>\0<20 bytes>`. What a mode
/// makes an entry follows git's `canon_mode`: the file-type bits say directory, or
/// regular file or symlink (both blobs), and whatever else is there, git takes for
/// a gitlink. The rest of the mode, how the entries are sorted and what the names
/// are is `git fsck`'s business, and old repositories hold trees it would complain
/// of.
fn tree_links(mut b: &[u8]) -> Result<Vec<Link>, Error> {
    let mut out = Vec::new();
    while !b.is_empty() {
        let sp = match b.iter().position(|&c| c == b' ') {
            Some(sp) if sp > 0 => sp,
            _ => return Err(malformed("tree entry without a mode")),
        };
        let mut mode = 0u32;
        for &c in &b[..sp] {
            if !(b'0'..=b'7').contains(&c) {
                return Err(malformed("tree entry with a mode that is not octal"));
            }
            // Shifting the high bits out, as Go's and C's unsigned arithmetic do.
            mode = mode << 3 | u32::from(c - b'0');
        }
        b = &b[sp + 1..];
        let Some(nul) = b.iter().position(|&c| c == 0) else {
            return Err(malformed("tree entry without an end to its name"));
        };
        if nul == 0 {
            return Err(malformed("tree entry with an empty name"));
        }
        b = &b[nul + 1..];
        if b.len() < GIT_SHA1_SIZE {
            return Err(malformed("tree entry with a short id"));
        }
        let mut sum = [0u8; GIT_SHA1_SIZE];
        sum.copy_from_slice(&b[..GIT_SHA1_SIZE]);
        b = &b[GIT_SHA1_SIZE..];
        let key = Key::new_git_sha1(sum);
        match mode & MODE_TYPE_MASK {
            MODE_DIR => out.push(Link { key, leaf: false }),
            MODE_REGULAR | MODE_SYMLINK => out.push(Link { key, leaf: true }),
            _ => {}
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The ids below are what git 2.50 gives these objects (Go: `gitobj_test.go`).
    const OID_EMPTY_BLOB: &str = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";
    const OID_HELLO: &str = "ce013625030ba8dba906f756967f9e9ca394464a";
    const OID_EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    const OID_SUB_TREE: &str = "84866613e022547daff138abd5b650c9cb9a9a66";
    const OID_TREE: &str = "49dff37cdebcd729bcfa301ce7b8b5b8e57da82e";
    const OID_COMMIT: &str = "15681690adcc0ebdfbf7a6ddc67a410536538e08";
    const OID_TAG: &str = "2a9fcefefd79daa213f6c0e533b8468f56eb59ab";

    fn raw(oid: &str) -> Vec<u8> {
        (0..oid.len() / 2)
            .map(|i| u8::from_str_radix(&oid[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    }

    fn git_key(oid: &str) -> Key {
        let mut sum = [0u8; GIT_SHA1_SIZE];
        sum.copy_from_slice(&raw(oid));
        Key::new_git_sha1(sum)
    }

    fn commit_body() -> String {
        format!(
            "tree {OID_TREE}\nauthor a <a@b> 1700000000 +0000\ncommitter a <a@b> 1700000000 +0000\n\none\n"
        )
    }

    fn tag_body() -> String {
        format!("object {OID_COMMIT}\ntype commit\ntag v1\ntagger a <a@b> 1700000000 +0000\n\nx\n")
    }

    fn tree_body() -> Vec<u8> {
        let mut b = b"40000 dir\0".to_vec();
        b.extend(raw(OID_SUB_TREE));
        b.extend(b"100644 file\0");
        b.extend(raw(OID_HELLO));
        b
    }

    fn is_malformed<T: fmt::Debug>(r: Result<T, Error>) -> bool {
        matches!(r, Err(Error::Malformed(_)))
    }

    #[test]
    fn encode_frames() {
        assert_eq!(encode(Kind::Blob, b"hello\n"), b"blob 6\0hello\n");
        assert_eq!(encode(Kind::Tree, b""), b"tree 0\0");
    }

    #[test]
    fn key_of_matches_git() {
        let cases: Vec<(Kind, Vec<u8>, &str)> = vec![
            (Kind::Blob, Vec::new(), OID_EMPTY_BLOB),
            (Kind::Blob, b"hello\n".to_vec(), OID_HELLO),
            (Kind::Tree, Vec::new(), OID_EMPTY_TREE),
            (Kind::Tree, tree_body(), OID_TREE),
            (Kind::Commit, commit_body().into_bytes(), OID_COMMIT),
            (Kind::Tag, tag_body().into_bytes(), OID_TAG),
        ];
        for (kind, body, oid) in cases {
            assert_eq!(key_of(&encode(kind, &body)), Ok(git_key(oid)), "{kind}");
        }
    }

    #[test]
    fn parse_frames() {
        let cases: &[(&[u8], Kind, &[u8])] = &[
            (b"blob 0\0", Kind::Blob, b""),
            (b"blob 3\0a\0b", Kind::Blob, b"a\0b"),
            (b"tree 0\0", Kind::Tree, b""),
            (b"commit 1\0x", Kind::Commit, b"x"),
            (b"tag 1\0x", Kind::Tag, b"x"),
        ];
        for (data, kind, body) in cases {
            assert_eq!(parse(data), Ok((*kind, *body)));
        }
    }

    #[test]
    fn parse_malformed() {
        let cases: &[(&str, &[u8])] = &[
            ("empty", b""),
            ("no space", b"blob"),
            ("no NUL", b"blob 3"),
            ("unknown kind", b"file 0\0"),
            ("capital kind", b"Blob 0\0"),
            ("empty kind", b" 0\0"),
            ("empty size", b"blob \0"),
            ("leading zero", b"blob 01\0x"),
            ("two zeros", b"blob 00\0"),
            ("sign", b"blob +1\0x"),
            ("letter in size", b"blob 1a\0x"),
            ("space in size", b"blob  1\0x"),
            ("size too large", b"blob 2\0x"),
            ("size too small", b"blob 1\0xy"),
            ("size overflows", b"blob 18446744073709551616\0"),
            ("size overflows 64", b"blob 99999999999999999999999\0"),
        ];
        for (name, data) in cases {
            assert!(is_malformed(parse(data)), "{name}");
        }
    }

    fn link(oid: &str, leaf: bool) -> Link {
        Link {
            key: git_key(oid),
            leaf,
        }
    }

    fn links_of(kind: Kind, body: &[u8]) -> Vec<Link> {
        links(&encode(kind, body)).unwrap()
    }

    #[test]
    fn links_blob() {
        // Content that reads like a commit is still a blob's.
        assert_eq!(links_of(Kind::Blob, commit_body().as_bytes()), vec![]);
    }

    #[test]
    fn links_commit() {
        assert_eq!(
            links_of(Kind::Commit, commit_body().as_bytes()),
            vec![link(OID_TREE, false)]
        );
    }

    #[test]
    fn links_commit_parents_in_order() {
        let body = format!(
            "tree {OID_TREE}\nparent {OID_COMMIT}\nparent {OID_TAG}\nauthor a <a@b> 1 +0000\ncommitter a <a@b> 1 +0000\n\nm\n"
        );
        assert_eq!(
            links_of(Kind::Commit, body.as_bytes()),
            vec![
                link(OID_TREE, false),
                link(OID_COMMIT, false),
                link(OID_TAG, false)
            ]
        );
    }

    /// Headers after the author are not links, whatever they look like.
    #[test]
    fn links_commit_only_leading_headers() {
        let body = format!(
            "tree {OID_TREE}\nauthor a <a@b> 1 +0000\ncommitter a <a@b> 1 +0000\nmergetag object {OID_TAG}\n type commit\n tag v1\ngpgsig -----BEGIN-----\n xyz\n\nparent {OID_COMMIT}\n"
        );
        assert_eq!(
            links_of(Kind::Commit, body.as_bytes()),
            vec![link(OID_TREE, false)]
        );
    }

    /// git takes a `parent ` line for one only if it can hold an id: what is too
    /// short to is not a parent and not an error, and git reads the commit (git
    /// 2.50, `parse_commit_buffer`). What is long enough has to be a parent.
    #[test]
    fn links_commit_short_final_parent_line_is_no_parent() {
        let tree = link(OID_TREE, false);
        for tail in [
            "parent abc\n".to_string(),
            "parent \n".to_string(),
            format!("parent {OID_COMMIT}"),
            format!("parent {}", &OID_COMMIT[..20]),
            "parent ".to_string(),
        ] {
            let body = format!("tree {OID_TREE}\n{tail}");
            assert_eq!(
                links_of(Kind::Commit, body.as_bytes()),
                vec![tree],
                "{tail}"
            );
        }
        // After a real parent as well.
        let body = format!("tree {OID_TREE}\nparent {OID_COMMIT}\nparent abc\n");
        assert_eq!(
            links_of(Kind::Commit, body.as_bytes()),
            vec![tree, link(OID_COMMIT, false)]
        );
    }

    /// git copies a tag's type line and compares it as a C string
    /// (`parse_tag_buffer`).
    #[test]
    fn links_tag_type_ends_at_a_nul() {
        let body = format!("object {OID_COMMIT}\ntype commit\0junk\ntag v\n\nm\n");
        assert_eq!(
            links_of(Kind::Tag, body.as_bytes()),
            vec![link(OID_COMMIT, false)]
        );
        let body = format!("object {OID_HELLO}\ntype blob\0junk\ntag v\n\nm\n");
        assert_eq!(
            links_of(Kind::Tag, body.as_bytes()),
            vec![link(OID_HELLO, true)]
        );
    }

    #[test]
    fn links_commit_needs_no_author_and_no_lowercase() {
        let want = vec![link(OID_TREE, false)];
        let bare = format!("tree {OID_TREE}\n");
        assert_eq!(links_of(Kind::Commit, bare.as_bytes()), want);
        let upper = format!("tree {}\n", OID_TREE.to_uppercase());
        assert_eq!(links_of(Kind::Commit, upper.as_bytes()), want);
    }

    #[test]
    fn links_tree() {
        assert_eq!(
            links_of(Kind::Tree, &tree_body()),
            vec![link(OID_SUB_TREE, false), link(OID_HELLO, true)]
        );
        assert_eq!(links_of(Kind::Tree, b""), vec![]);
    }

    fn entry(mode: &str, name: &[u8], oid: &str) -> Vec<u8> {
        let mut b = mode.as_bytes().to_vec();
        b.push(b' ');
        b.extend_from_slice(name);
        b.push(0);
        b.extend(raw(oid));
        b
    }

    /// A mode decides what an entry is the way git's `canon_mode` does.
    #[test]
    fn links_tree_modes() {
        let blob = vec![link(OID_HELLO, true)];
        let dir = vec![link(OID_HELLO, false)];
        let cases: Vec<(&str, Vec<Link>)> = vec![
            ("100644", blob.clone()),
            ("100755", blob.clone()),
            ("100664", blob.clone()), // old repositories
            ("120000", blob.clone()), // a symlink's target is a blob
            ("40000", dir.clone()),
            ("040000", dir.clone()), // old tools padded it
            ("160000", vec![]),      // a submodule's commit, in another repository
            ("0", vec![]),           // anything else git takes for a gitlink as well
            ("60000", vec![]),
            ("7", vec![]),
            ("300644", blob.clone()), // only the file-type bits count, and only four of them
            ("240000", dir.clone()),
        ];
        for (mode, want) in cases {
            assert_eq!(
                links_of(Kind::Tree, &entry(mode, b"x", OID_HELLO)),
                want,
                "{mode}"
            );
        }
    }

    /// A mode long enough to shift bits out reads as in Go: this one's low bits
    /// say regular file. It must not panic either.
    #[test]
    fn links_tree_mode_overflow_shifts_out() {
        assert_eq!(
            links_of(
                Kind::Tree,
                &entry("7777777777777777100644", b"x", OID_HELLO)
            ),
            vec![link(OID_HELLO, true)]
        );
    }

    /// A name holds any byte but NUL, spaces and newlines among them.
    #[test]
    fn links_tree_names() {
        assert_eq!(
            links_of(Kind::Tree, &entry("100644", b"a b\nc", OID_HELLO)),
            vec![link(OID_HELLO, true)]
        );
    }

    #[test]
    fn links_tag() {
        for (target, leaf) in [
            ("commit", false),
            ("tree", false),
            ("tag", false),
            ("blob", true),
        ] {
            let body = format!("object {OID_HELLO}\ntype {target}\ntag v\n\nm\n");
            assert_eq!(
                links_of(Kind::Tag, body.as_bytes()),
                vec![link(OID_HELLO, leaf)],
                "{target}"
            );
        }
    }

    #[test]
    fn links_malformed() {
        let h = raw(OID_HELLO);
        let z = "z".repeat(40);
        let with_id = |prefix: &[u8], id: &[u8], suffix: &[u8]| {
            let mut b = prefix.to_vec();
            b.extend_from_slice(id);
            b.extend_from_slice(suffix);
            b
        };
        let cases: Vec<(&str, Kind, Vec<u8>)> = vec![
            ("commit empty", Kind::Commit, Vec::new()),
            (
                "commit without tree",
                Kind::Commit,
                b"author a <a@b> 1 +0000\n".to_vec(),
            ),
            (
                "commit tree short",
                Kind::Commit,
                format!("tree {}\n", &OID_TREE[..39]).into_bytes(),
            ),
            (
                "commit tree not hex",
                Kind::Commit,
                format!("tree {z}\n").into_bytes(),
            ),
            (
                "commit tree no newline",
                Kind::Commit,
                format!("tree {OID_TREE}").into_bytes(),
            ),
            (
                "commit tree line long",
                Kind::Commit,
                format!("tree {OID_TREE}0\n").into_bytes(),
            ),
            (
                "commit parent long enough, not one",
                Kind::Commit,
                format!(
                    "tree {OID_TREE}\nparent abc\nauthor a <a@b> 1 +0000\ncommitter a <a@b> 1 +0000\n\nm\n"
                )
                .into_bytes(),
            ),
            (
                "commit parent not hex",
                Kind::Commit,
                format!("tree {OID_TREE}\nparent {z}\n").into_bytes(),
            ),
            (
                "commit parent line long",
                Kind::Commit,
                format!("tree {OID_TREE}\nparent {OID_COMMIT}0\n").into_bytes(),
            ),
            ("tree no space", Kind::Tree, b"100644".to_vec()),
            ("tree empty mode", Kind::Tree, with_id(b" x\0", &h, b"")),
            (
                "tree mode not octal",
                Kind::Tree,
                with_id(b"100648 x\0", &h, b""),
            ),
            (
                "tree mode not digits",
                Kind::Tree,
                with_id(b"abc x\0", &h, b""),
            ),
            ("tree no NUL", Kind::Tree, b"100644 x".to_vec()),
            (
                "tree empty name",
                Kind::Tree,
                with_id(b"100644 \0", &h, b""),
            ),
            (
                "tree id short",
                Kind::Tree,
                with_id(b"100644 x\0", &h[..19], b""),
            ),
            (
                "tree trailing",
                Kind::Tree,
                with_id(b"100644 x\0", &h, b"1"),
            ),
            ("tag empty", Kind::Tag, Vec::new()),
            ("tag without object", Kind::Tag, b"type commit\n".to_vec()),
            (
                "tag object short",
                Kind::Tag,
                b"object abc\ntype commit\n".to_vec(),
            ),
            (
                "tag object not hex",
                Kind::Tag,
                format!("object {z}\ntype commit\n").into_bytes(),
            ),
            (
                "tag object no newline",
                Kind::Tag,
                format!("object {OID_COMMIT}").into_bytes(),
            ),
            (
                "tag without type",
                Kind::Tag,
                format!("object {OID_COMMIT}\ntag v\n").into_bytes(),
            ),
            (
                "tag type unknown",
                Kind::Tag,
                format!("object {OID_COMMIT}\ntype file\n").into_bytes(),
            ),
            (
                "tag type no newline",
                Kind::Tag,
                format!("object {OID_COMMIT}\ntype commit").into_bytes(),
            ),
            (
                "tag type empty before the NUL",
                Kind::Tag,
                format!("object {OID_COMMIT}\ntype \0commit\n").into_bytes(),
            ),
        ];
        for (name, kind, body) in cases {
            assert!(is_malformed(links(&encode(kind, &body))), "{name}");
        }
        assert!(is_malformed(links(b"not an object")));
    }

    #[test]
    fn verify_accepts_the_object_its_key_names() {
        let data = encode(Kind::Commit, commit_body().as_bytes());
        assert_eq!(verify(git_key(OID_COMMIT), &data), Ok(()));
    }

    #[test]
    fn verify_wrong_key() {
        let data = encode(Kind::Commit, commit_body().as_bytes());
        assert!(matches!(
            verify(git_key(OID_HELLO), &data),
            Err(Error::Mismatch { .. })
        ));
    }

    /// The hash is right, and the payload is not a git object at all: it has no
    /// frame.
    #[test]
    fn verify_refuses_what_has_no_frame() {
        let cases: [(&str, &[u8]); 3] = [
            ("not framed", b"hello"),
            ("size wrong", b"blob 9\0x"),
            ("unknown kind", b"file 1\0x"),
        ];
        for (name, data) in cases {
            let k = key_of(data).unwrap();
            assert!(is_malformed(verify(k, data)), "{name}");
        }
    }

    /// What an object holds is for whoever reads it. git never parses an object it
    /// only reaches as a file, and a repository may well hold one that would not
    /// parse: the walk that does read it refuses it then, and nothing short of that
    /// may.
    #[test]
    fn verify_leaves_the_content_to_the_walks() {
        let odd_tag = format!("object {OID_COMMIT}\ntype file\n");
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("commit is junk", encode(Kind::Commit, b"junk")),
            ("tree is junk", encode(Kind::Tree, b"junk")),
            ("tag is junk", encode(Kind::Tag, b"junk")),
            ("tag type is odd", encode(Kind::Tag, odd_tag.as_bytes())),
        ];
        for (name, data) in cases {
            let k = key_of(&data).unwrap();
            assert_eq!(verify(k, &data), Ok(()), "{name}");
            assert!(is_malformed(links(&data)), "{name}");
        }
    }

    // The first 320 bytes of shattered-1.pdf: the prefix and the two blocks of
    // the first public SHA-1 collision, enough for the detection to fire. Plain
    // SHA-1 gives this f92d74e3874587aaf443d1db961d4e26dde13e9c.
    const SHATTERED_PREFIX: &str = concat!(
        "255044462d312e330a25e2e3cfd30a0a0a312030206f626a0a3c3c2f57696474",
        "682032203020522f4865696768742033203020522f547970652034203020522f",
        "537562747970652035203020522f46696c7465722036203020522f436f6c6f72",
        "53706163652037203020522f4c656e6774682038203020522f42697473506572",
        "436f6d706f6e656e7420383e3e0a73747265616d0affd8fffe00245348412d31",
        "20697320646561642121212121852fec092339759c39b1a1c63c4c97e1fffe01",
        "7346dc9166b67e118f029ab621b2560ff9ca67cca8c7f85ba84c79030c2b3de2",
        "18f86db3a90901d5df45c14f26fedfb3dc38e96ac22fe7bd728f0e45bce046d2",
        "3c570feb141398bb552ef5a0a82be331fea48037b8b5d71f0e332edf93ac3500",
        "eb4ddc0decc1a864790c782c76215660dd309791d06bd0af3f98cda4bc4629b1",
    );

    #[test]
    fn collision_is_refused() {
        let data = raw(SHATTERED_PREFIX);
        assert_eq!(key_of(&data), Err(Error::Collision));
        // Checked before anything else, whatever key is claimed.
        let claimed = git_key("f92d74e3874587aaf443d1db961d4e26dde13e9c");
        assert_eq!(verify(claimed, &data), Err(Error::Collision));
    }

    #[test]
    fn error_text_matches_go() {
        assert_eq!(
            Error::Collision.to_string(),
            "gitobj: SHA-1 collision attack detected"
        );
        assert_eq!(
            malformed("unknown kind").to_string(),
            "gitobj: malformed object: unknown kind"
        );
        let k = git_key(OID_HELLO);
        assert_eq!(
            Error::Mismatch { want: k, got: k }.to_string(),
            format!("gitobj: payload does not match its key: payload hashes to {k}, not {k}")
        );
    }
}
