//! Object encoders: byte-identical to Go's fxamacker core-deterministic
//! encoding (with `NilContainerAsEmpty`) of the `fstree` shapes.

use std::collections::BTreeMap;

use super::{DirPair, Entry, Error, Object, fx};
use crate::cbor::{MAJOR_ARRAY, MAJOR_MAP, MAJOR_NEGINT, MAJOR_UINT, append_bstr, append_head};
use crate::key::{Key, SIZE, Type};

/// Appends `v` as a CBOR integer: major 0 for non-negative values, major 1
/// with the argument `-1 - v` otherwise (fxamacker `int64` encoding).
fn append_int(out: &mut Vec<u8>, v: i64) {
    if v >= 0 {
        append_head(out, MAJOR_UINT, v as u64);
    } else {
        append_head(out, MAJOR_NEGINT, !v as u64);
    }
}

/// Appends one entry map. Keys 0–4 are always present; 5–9 are omitted when
/// empty (Go `omitempty`: an empty slice and nil are both "empty").
fn append_entry(out: &mut Vec<u8>, e: &Entry) -> Result<(), Error> {
    let mut pairs = 5u64;
    for present in [
        !e.content_key.is_empty(),
        !e.link_target.is_empty(),
        !e.rdev.is_empty(),
        !e.xattrs_in.is_empty(),
        !e.xattrs_key.is_empty(),
    ] {
        if present {
            pairs += 1;
        }
    }
    append_head(out, MAJOR_MAP, pairs);
    append_head(out, MAJOR_UINT, 0);
    append_bstr(out, &e.name);
    append_head(out, MAJOR_UINT, 1);
    append_head(out, MAJOR_UINT, e.mode);
    append_head(out, MAJOR_UINT, 2);
    append_head(out, MAJOR_UINT, e.uid);
    append_head(out, MAJOR_UINT, 3);
    append_head(out, MAJOR_UINT, e.gid);
    append_head(out, MAJOR_UINT, 4);
    append_int(out, e.mtime);
    if !e.content_key.is_empty() {
        append_head(out, MAJOR_UINT, 5);
        append_bstr(out, &e.content_key);
    }
    if !e.link_target.is_empty() {
        append_head(out, MAJOR_UINT, 6);
        append_bstr(out, &e.link_target);
    }
    if !e.rdev.is_empty() {
        append_head(out, MAJOR_UINT, 7);
        append_head(out, MAJOR_ARRAY, e.rdev.len() as u64);
        for &r in &e.rdev {
            append_head(out, MAJOR_UINT, r);
        }
    }
    if !e.xattrs_in.is_empty() {
        // fxamacker re-validates Marshaler output: exactly one well-formed
        // item, indefinite lengths forbidden, built-in tag content checked.
        fx::raw_message_wellformed(&e.xattrs_in).map_err(Error::MarshalRawMessage)?;
        append_head(out, MAJOR_UINT, 8);
        out.extend_from_slice(&e.xattrs_in);
    }
    if !e.xattrs_key.is_empty() {
        append_head(out, MAJOR_UINT, 9);
        append_bstr(out, &e.xattrs_key);
    }
    Ok(())
}

/// The canonical encoding of an entry array (Go `encMode.Marshal(entries)`),
/// without the key computation. Shared by [`encode_dir_leaf`] and tests.
pub(super) fn marshal_entries(entries: &[Entry]) -> Result<Vec<u8>, Error> {
    let mut b = Vec::new();
    append_head(&mut b, MAJOR_ARRAY, entries.len() as u64);
    for e in entries {
        append_entry(&mut b, e)?;
    }
    Ok(b)
}

/// The canonical encoding of a pair array (Go `encMode.Marshal(pairs)`),
/// without the key computation. Shared by [`encode_dir_node`] and tests.
pub(super) fn marshal_pairs(pairs: &[DirPair]) -> Vec<u8> {
    let mut b = Vec::new();
    append_head(&mut b, MAJOR_ARRAY, pairs.len() as u64);
    for p in pairs {
        append_head(&mut b, MAJOR_ARRAY, 2);
        append_bstr(&mut b, &p.sep_name);
        append_bstr(&mut b, &p.child_key);
    }
    b
}

/// Wraps raw file-content bytes as a `Blob` object (no CBOR framing).
///
/// Infallible, unlike Go's `EncodeBlob`: its only failure source
/// (`key.New` rejecting a reserved type) is unrepresentable here.
pub fn encode_blob(data: &[u8]) -> Object {
    Object {
        key: Key::new(Type::Blob, data.len() as u64, data),
        bytes: data.to_vec(),
    }
}

/// Encodes a file index node: a CBOR array of child keys. Its length field is
/// the sum of child content sizes (excludes its own bytes).
///
/// Infallible, unlike Go's `EncodeFileNode` (see [`encode_blob`]).
pub fn encode_file_node(children: &[Key]) -> Object {
    let mut b = Vec::with_capacity(1 + children.len() * (SIZE + 2));
    append_head(&mut b, MAJOR_ARRAY, children.len() as u64);
    let mut sum = 0u64;
    for c in children {
        append_bstr(&mut b, c.as_bytes());
        sum = sum.wrapping_add(c.length());
    }
    let key = Key::new(Type::FileNode, sum, &b);
    Object { key, bytes: b }
}

/// Encodes a run of directory entries (already sorted by name) as a CBOR
/// array of entry maps. Its length field is its own serialized bytes plus the
/// content-key length of each regular-file/directory entry plus the
/// xattrs-key length of each entry whose xattrs were spilled to an XattrSet.
pub fn encode_dir_leaf(entries: &[Entry]) -> Result<Object, Error> {
    let b = marshal_entries(entries)?;
    let mut sub = 0u64;
    for e in entries {
        if e.content_key.len() == SIZE {
            let ck = Key::parse(&e.content_key).map_err(|source| Error::EntryContentKey {
                name: e.name.clone(),
                source,
            })?;
            sub = sub.wrapping_add(ck.length());
        }
        if e.xattrs_key.len() == SIZE {
            let xk = Key::parse(&e.xattrs_key).map_err(|source| Error::EntryXattrsKey {
                name: e.name.clone(),
                source,
            })?;
            sub = sub.wrapping_add(xk.length());
        }
    }
    let key = Key::new(Type::DirLeaf, (b.len() as u64).wrapping_add(sub), &b);
    Ok(Object { key, bytes: b })
}

/// Encodes a directory index node as a CBOR array of `[sepName, childKey]`
/// pairs (sorted by sepName). Its length field is its own serialized bytes
/// plus the cumulative length of every child.
pub fn encode_dir_node(pairs: &[DirPair]) -> Result<Object, Error> {
    let b = marshal_pairs(pairs);
    let mut sub = 0u64;
    for p in pairs {
        let ck = Key::parse(&p.child_key).map_err(Error::DirNodeChildKey)?;
        sub = sub.wrapping_add(ck.length());
    }
    let key = Key::new(Type::DirNode, (b.len() as u64).wrapping_add(sub), &b);
    Ok(Object { key, bytes: b })
}

/// Encodes a spilled extended-attribute set. Its length field is its own
/// serialized byte length.
///
/// Infallible, unlike Go's `EncodeXattrSet` (see [`encode_blob`]).
pub fn encode_xattr_set(m: &BTreeMap<Vec<u8>, Vec<u8>>) -> Object {
    let b = crate::cbor::encode_xattrs(m);
    let key = Key::new(Type::XattrSet, b.len() as u64, &b);
    Object { key, bytes: b }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn unhex(s: &str) -> Vec<u8> {
        assert_eq!(s.len() % 2, 0, "odd hex length");
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
            .collect()
    }

    /// key.New(Blob, 100, "x"), the reference child key used by the Go
    /// oracle harness.
    fn k1() -> Key {
        let k = Key::new(Type::Blob, 100, b"x");
        assert_eq!(
            k.to_string(),
            "8d5497c63a49c025af4e80f66ba5856a09d40ab7ac02649a78f605d8e73a6400",
            "baseline key differs from the Go oracle"
        );
        k
    }

    const K1_BSTR: &str = "58208d5497c63a49c025af4e80f66ba5856a09d40ab7ac02649a78f605d8e73a6400";

    // Every case below reproduces a Go oracle output: encMode.Marshal with
    // fxamacker CoreDetEncOptions + NilContainerAsEmpty at cbor v2.9.2.
    #[test]
    fn marshal_entries_matches_fxamacker() {
        let k = k1();
        let cases: &[(&str, Vec<Entry>, &str)] = &[
            (
                "entry_zero",
                vec![Entry::default()],
                "81a500400100020003000400",
            ),
            (
                "entry_basic",
                vec![Entry {
                    name: b"f".to_vec(),
                    mode: 0o100644,
                    uid: 1000,
                    gid: 1000,
                    mtime: 1700000000000000000,
                    content_key: k.as_bytes().to_vec(),
                    ..Default::default()
                }],
                "81a6004166011981a4021903e8031903e8041b17979cfe362a00000558208d5497c63a49c025af4e80f66ba5856a09d40ab7ac02649a78f605d8e73a6400",
            ),
            (
                "entry_neg_mtime",
                vec![Entry {
                    name: b"f".to_vec(),
                    mtime: -1,
                    ..Default::default()
                }],
                "81a50041660100020003000420",
            ),
            (
                "entry_neg_mtime_min",
                vec![Entry {
                    name: b"f".to_vec(),
                    mtime: i64::MIN,
                    ..Default::default()
                }],
                "81a5004166010002000300043b7fffffffffffffff",
            ),
            (
                "entry_mtime_max",
                vec![Entry {
                    name: b"f".to_vec(),
                    mtime: i64::MAX,
                    ..Default::default()
                }],
                "81a5004166010002000300041b7fffffffffffffff",
            ),
            (
                "entry_mtime_small_neg",
                vec![Entry {
                    mtime: -24,
                    ..Default::default()
                }],
                "81a500400100020003000437",
            ),
            (
                "entry_mtime_neg25",
                vec![Entry {
                    mtime: -25,
                    ..Default::default()
                }],
                "81a50040010002000300043818",
            ),
            (
                "entry_rdev",
                vec![Entry {
                    rdev: vec![259, 0],
                    ..Default::default()
                }],
                "81a600400100020003000400078219010300",
            ),
            (
                "entry_rdev_one",
                vec![Entry {
                    rdev: vec![1],
                    ..Default::default()
                }],
                "81a600400100020003000400078101",
            ),
            (
                "entry_rdev_three",
                vec![Entry {
                    rdev: vec![1, 2, 3],
                    ..Default::default()
                }],
                "81a6004001000200030004000783010203",
            ),
            (
                "entry_rdev_big",
                vec![Entry {
                    rdev: vec![u64::MAX],
                    ..Default::default()
                }],
                "81a60040010002000300040007811bffffffffffffffff",
            ),
            (
                "entry_linktarget",
                vec![Entry {
                    link_target: b"../small.txt".to_vec(),
                    ..Default::default()
                }],
                "81a600400100020003000400064c2e2e2f736d616c6c2e747874",
            ),
            (
                "entry_xattrs_in",
                vec![Entry {
                    xattrs_in: vec![0xa1, 0x41, 0x61, 0x41, 0x62],
                    ..Default::default()
                }],
                "81a60040010002000300040008a141614162",
            ),
            (
                // A well-formed but non-canonical RawMessage splices verbatim.
                "entry_xattrs_in_noncanon",
                vec![Entry {
                    xattrs_in: vec![0xb8, 0x01, 0x41, 0x61, 0x41, 0x62],
                    ..Default::default()
                }],
                "81a60040010002000300040008b80141614162",
            ),
            (
                "entry_xattrs_key",
                vec![Entry {
                    xattrs_key: k.as_bytes().to_vec(),
                    ..Default::default()
                }],
                "81a6004001000200030004000958208d5497c63a49c025af4e80f66ba5856a09d40ab7ac02649a78f605d8e73a6400",
            ),
            (
                "entry_both_xattrs",
                vec![Entry {
                    xattrs_in: vec![0xa0],
                    xattrs_key: k.as_bytes().to_vec(),
                    ..Default::default()
                }],
                "81a70040010002000300040008a00958208d5497c63a49c025af4e80f66ba5856a09d40ab7ac02649a78f605d8e73a6400",
            ),
            (
                "entry_mode_boundaries",
                [
                    23u64,
                    24,
                    255,
                    256,
                    65535,
                    65536,
                    4294967295,
                    4294967296,
                    u64::MAX,
                ]
                .iter()
                .map(|&mode| Entry {
                    mode,
                    ..Default::default()
                })
                .collect(),
                "89a500400117020003000400a50040011818020003000400a500400118ff020003000400a5004001190100020003000400a500400119ffff020003000400a50040011a00010000020003000400a50040011affffffff020003000400a50040011b0000000100000000020003000400a50040011bffffffffffffffff020003000400",
            ),
            ("entries_empty", vec![], "80"),
            (
                "name_long",
                vec![Entry {
                    name: vec![0u8; 24],
                    ..Default::default()
                }],
                "81a50058180000000000000000000000000000000000000000000000000100020003000400",
            ),
            (
                // fxamacker RawMessage validation allows floats, tags,
                // null, non-shortest heads, invalid-UTF-8 text, and deep
                // nesting (no depth limit on the marshaler check).
                "entry_xattrs_in_float",
                vec![Entry {
                    xattrs_in: unhex("fb3ff0000000000000"),
                    ..Default::default()
                }],
                "81a60040010002000300040008fb3ff0000000000000",
            ),
            (
                "entry_xattrs_in_tag",
                vec![Entry {
                    xattrs_in: unhex("d8184100"),
                    ..Default::default()
                }],
                "81a60040010002000300040008d8184100",
            ),
            (
                "entry_xattrs_in_null",
                vec![Entry {
                    xattrs_in: vec![0xf6],
                    ..Default::default()
                }],
                "81a60040010002000300040008f6",
            ),
            (
                "entry_xattrs_in_nonshortest",
                vec![Entry {
                    xattrs_in: vec![0x18, 0x05],
                    ..Default::default()
                }],
                "81a600400100020003000400081805",
            ),
            (
                "entry_xattrs_in_invalid_utf8_tstr",
                vec![Entry {
                    xattrs_in: vec![0x61, 0xff],
                    ..Default::default()
                }],
                "81a6004001000200030004000861ff",
            ),
            (
                "entry_xattrs_in_deep33",
                vec![Entry {
                    xattrs_in: {
                        let mut v = vec![0x81u8; 33];
                        v.push(0x40);
                        v
                    },
                    ..Default::default()
                }],
                "81a6004001000200030004000881818181818181818181818181818181818181818181818181818181818181818140",
            ),
        ];
        for (name, entries, want) in cases {
            let got = marshal_entries(entries).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(hex(&got), *want, "{name}");
        }
    }

    #[test]
    fn marshal_entries_empty_optionals_omitted() {
        // Empty (not just missing) optional fields are omitted, like Go's
        // omitempty treats zero-length slices.
        let e = Entry {
            content_key: Vec::new(),
            link_target: Vec::new(),
            rdev: Vec::new(),
            xattrs_in: Vec::new(),
            xattrs_key: Vec::new(),
            ..Default::default()
        };
        assert_eq!(
            hex(&marshal_entries(&[e]).unwrap()),
            "81a500400100020003000400"
        );
    }

    #[test]
    fn marshal_entries_name_255_and_256() {
        let e255 = Entry {
            name: vec![0u8; 255],
            ..Default::default()
        };
        let got = marshal_entries(&[e255]).unwrap();
        assert!(got.starts_with(&unhex("81a50058ff")), "255-byte name head");
        let e256 = Entry {
            name: vec![0u8; 256],
            ..Default::default()
        };
        let got = marshal_entries(&[e256]).unwrap();
        assert!(
            got.starts_with(&unhex("81a500590100")),
            "256-byte name head"
        );
    }

    #[test]
    fn marshal_entries_raw_message_errors() {
        // fxamacker MarshalerError strings, byte-for-byte.
        let cases: &[(&str, Vec<u8>, &str)] = &[
            (
                "invalid",
                vec![0xff, 0xff],
                "cbor: error calling MarshalCBOR for type cbor.RawMessage: cbor: unexpected \"break\" code",
            ),
            (
                "trailing",
                vec![0xa0, 0x00],
                "cbor: error calling MarshalCBOR for type cbor.RawMessage: cbor: 1 bytes of extraneous data starting at index 1",
            ),
            (
                "indef_array",
                vec![0x9f, 0xff],
                "cbor: error calling MarshalCBOR for type cbor.RawMessage: cbor: indefinite-length array isn't allowed",
            ),
            (
                "indef_bstr",
                vec![0x5f, 0x41, 0x61, 0xff],
                "cbor: error calling MarshalCBOR for type cbor.RawMessage: cbor: indefinite-length byte string isn't allowed",
            ),
            (
                "indef_tstr",
                vec![0x7f, 0x61, 0x61, 0xff],
                "cbor: error calling MarshalCBOR for type cbor.RawMessage: cbor: indefinite-length UTF-8 text string isn't allowed",
            ),
            (
                "truncated",
                vec![0xa1],
                "cbor: error calling MarshalCBOR for type cbor.RawMessage: unexpected EOF",
            ),
            (
                "reserved_ai",
                vec![0x1c],
                "cbor: error calling MarshalCBOR for type cbor.RawMessage: cbor: invalid additional information 28 for type positive integer",
            ),
        ];
        for (name, raw, want) in cases {
            let e = Entry {
                xattrs_in: raw.clone(),
                ..Default::default()
            };
            let err = encode_dir_leaf(&[e]).expect_err(name);
            assert_eq!(err.to_string(), *want, "{name}");
            assert!(
                matches!(err, Error::MarshalRawMessage(_)),
                "{name}: variant"
            );
        }
    }

    // Ports of the Go encode_test.go tests.

    #[test]
    fn encode_blob_length_is_byte_count() {
        let o = encode_blob(b"hello");
        assert_eq!(o.key.type_(), Type::Blob);
        assert_eq!(o.key.length(), 5);
        assert_eq!(o.bytes, b"hello");
    }

    #[test]
    fn encode_blob_empty_matches_go() {
        let o = encode_blob(&[]);
        assert_eq!(
            o.key.to_string(),
            "1fe4ca939accb712c1adc925cb9b49c9dc36ea4d40a0a6a1f9f5b94913af0000"
        );
        assert_eq!(o.key.length(), 0);
        assert!(o.bytes.is_empty());
    }

    #[test]
    fn encode_file_node_length_is_sum_of_children() {
        let a = encode_blob(&[0u8; 100]);
        let b = encode_blob(&[0u8; 250]);
        let o = encode_file_node(&[a.key, b.key]);
        assert_eq!(o.key.type_(), Type::FileNode);
        assert_eq!(o.key.length(), 350, "excludes the FileNode's own bytes");
    }

    #[test]
    fn encode_file_node_matches_go_bytes() {
        let k = k1();
        let o = encode_file_node(&[k, k]);
        assert_eq!(hex(&o.bytes), format!("82{K1_BSTR}{K1_BSTR}"));
        assert_eq!(o.key.length(), 200);
        assert_eq!(encode_file_node(&[]).bytes, vec![0x80]);
    }

    #[test]
    fn encode_dir_leaf_length_is_own_bytes_plus_content_keys() {
        let child = encode_blob(&[0u8; 1000]);
        let e = Entry {
            name: b"f".to_vec(),
            mode: 0o100644,
            content_key: child.key.as_bytes().to_vec(),
            ..Default::default()
        };
        let o = encode_dir_leaf(&[e]).unwrap();
        assert_eq!(o.key.type_(), Type::DirLeaf);
        assert_eq!(o.key.length(), o.bytes.len() as u64 + 1000);
    }

    #[test]
    fn encode_dir_leaf_symlink_adds_only_own_bytes() {
        let e = Entry {
            name: b"l".to_vec(),
            mode: 0o120777,
            link_target: b"target/path".to_vec(),
            ..Default::default()
        };
        let o = encode_dir_leaf(&[e]).unwrap();
        assert_eq!(o.key.length(), o.bytes.len() as u64);
    }

    #[test]
    fn encode_dir_leaf_short_content_key_not_counted() {
        // A content key that is not exactly 32 bytes is not parsed and adds
        // nothing (mirrors Go's len(e.ContentKey) == key.Size guard).
        let e = Entry {
            name: b"f".to_vec(),
            content_key: vec![0u8; 31],
            ..Default::default()
        };
        let o = encode_dir_leaf(&[e]).unwrap();
        assert_eq!(o.key.length(), o.bytes.len() as u64);
        assert_eq!(o.key.length(), 47, "oracle: len=47 own=47");
    }

    #[test]
    fn encode_dir_leaf_bad_keys_error_text() {
        let mut bad = vec![0u8; 32];
        bad[31] = 0x08; // reserved header bit
        let e = Entry {
            name: b"f".to_vec(),
            content_key: bad.clone(),
            ..Default::default()
        };
        let err = encode_dir_leaf(&[e]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "entry \"f\" content key: key: reserved header bit is set"
        );
        let e = Entry {
            name: b"f".to_vec(),
            xattrs_key: bad,
            ..Default::default()
        };
        let err = encode_dir_leaf(&[e]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "entry \"f\" xattrs key: key: reserved header bit is set"
        );
    }

    #[test]
    fn encode_dir_node_length_is_own_bytes_plus_children() {
        let c1 = encode_dir_leaf(&[Entry {
            name: b"a".to_vec(),
            mode: 0o40755,
            ..Default::default()
        }])
        .unwrap();
        let c2 = encode_dir_leaf(&[Entry {
            name: b"b".to_vec(),
            mode: 0o40755,
            ..Default::default()
        }])
        .unwrap();
        let o = encode_dir_node(&[
            DirPair {
                sep_name: b"a".to_vec(),
                child_key: c1.key.as_bytes().to_vec(),
            },
            DirPair {
                sep_name: b"b".to_vec(),
                child_key: c2.key.as_bytes().to_vec(),
            },
        ])
        .unwrap();
        assert_eq!(
            o.key.length(),
            o.bytes.len() as u64 + c1.key.length() + c2.key.length()
        );
    }

    #[test]
    fn encode_dir_node_matches_go_bytes() {
        let k = k1();
        let o = encode_dir_node(&[DirPair {
            sep_name: b"a".to_vec(),
            child_key: k.as_bytes().to_vec(),
        }])
        .unwrap();
        assert_eq!(hex(&o.bytes), format!("81824161{K1_BSTR}"));
        assert_eq!(marshal_pairs(&[DirPair::default()]), unhex("81824040"));
        assert_eq!(marshal_pairs(&[]), vec![0x80]);
    }

    #[test]
    fn encode_dir_node_bad_keys_error_text() {
        let mut bad = vec![0u8; 32];
        bad[31] = 0x08;
        let err = encode_dir_node(&[DirPair {
            sep_name: b"a".to_vec(),
            child_key: bad,
        }])
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "dir node child key: key: reserved header bit is set"
        );
        // Unlike DirLeaf, a short child key is always parsed (and rejected).
        let err = encode_dir_node(&[DirPair {
            sep_name: b"a".to_vec(),
            child_key: vec![0u8; 31],
        }])
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "dir node child key: key: data is not 32 bytes: got 31"
        );
    }

    #[test]
    fn encode_xattr_set_length_is_own_bytes() {
        let mut m = BTreeMap::new();
        m.insert(b"user.a".to_vec(), b"v".to_vec());
        let o = encode_xattr_set(&m);
        assert_eq!(o.key.type_(), Type::XattrSet);
        assert_eq!(o.key.length(), o.bytes.len() as u64);
        assert_eq!(o.bytes, crate::cbor::encode_xattrs(&m));
    }

    #[test]
    fn encode_dir_leaf_inline_xattrs_embedded_verbatim() {
        let mut m = BTreeMap::new();
        m.insert(b"user.x".to_vec(), b"y".to_vec());
        let inline = crate::cbor::encode_xattrs(&m);
        let e = Entry {
            name: b"f".to_vec(),
            mode: 0o100644,
            xattrs_in: inline.clone(),
            ..Default::default()
        };
        let o = encode_dir_leaf(&[e]).unwrap();
        assert!(
            o.bytes
                .windows(inline.len())
                .any(|w| w == inline.as_slice()),
            "inline xattrs not embedded verbatim"
        );
    }

    #[test]
    fn encoders_deterministic() {
        let e = Entry {
            name: b"z".to_vec(),
            mode: 0o100644,
            uid: 1000,
            gid: 1000,
            mtime: -5,
            ..Default::default()
        };
        let a = encode_dir_leaf(std::slice::from_ref(&e)).unwrap();
        let b = encode_dir_leaf(&[e]).unwrap();
        assert_eq!(a.key, b.key, "DirLeaf encoding not deterministic");
    }

    #[test]
    fn encode_dir_leaf_empty_is_canonical_empty_array() {
        let o = encode_dir_leaf(&[]).unwrap();
        assert_eq!(o.bytes, vec![0x80]);
    }

    /// `xattrs_in` nested `n` arrays deep around an empty byte string.
    fn deep_raw(n: usize) -> Vec<u8> {
        let mut v = vec![0x81u8; n];
        v.push(0x40);
        v
    }

    #[test]
    fn raw_message_marshaler_nesting_limit() {
        // Go oracle (pinned commit): depth 65535 marshals fine — fxamacker
        // validates Marshaler output with its limits maxed out — and 65536
        // exceeds them. Also proves the iterative wellformed check survives
        // 65535 levels without exhausting the thread stack.
        let e = Entry {
            name: b"f".to_vec(),
            xattrs_in: deep_raw(65535),
            ..Default::default()
        };
        encode_dir_leaf(&[e]).expect("depth 65535 must be accepted");

        let e = Entry {
            name: b"f".to_vec(),
            xattrs_in: deep_raw(65536),
            ..Default::default()
        };
        let err = encode_dir_leaf(&[e]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "cbor: error calling MarshalCBOR for type cbor.RawMessage: \
             cbor: exceeded max nested level 65535"
        );
    }

    #[test]
    fn raw_message_marshaler_allows_arrays_over_decode_cap() {
        // A definite 131073-element array exceeds the *decode* cap (131072)
        // but not the marshaler-validation cap (2147483647), so it splices.
        // Key and length pinned from the Go oracle at the pinned commit.
        let mut big = vec![0x9a, 0x00, 0x02, 0x00, 0x01];
        big.extend(std::iter::repeat_n(0u8, 131073));
        let e = Entry {
            name: b"f".to_vec(),
            xattrs_in: big,
            ..Default::default()
        };
        let o = encode_dir_leaf(&[e]).expect("marshaler cap is 2147483647");
        assert_eq!(o.bytes.len(), 131092);
        assert_eq!(
            o.key.to_string(),
            "5d6de59b69f7e1449db1fec9b61c07e284af6c2e8a3f2f711074c9f614000222"
        );
    }
}
