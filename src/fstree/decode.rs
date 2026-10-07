//! Object decoders, mirroring Go `fstree/decode.go`'s exact acceptance
//! behavior (default fxamacker `cbor.Unmarshal`; see the `fx` module).

use super::{DirPair, Entry, Error, fx};
use crate::key::Key;

/// Decodes a FileNode body (a CBOR array of child keys) into its child keys,
/// in file order. The inverse of [`super::encode_file_node`].
pub fn decode_file_node(b: &[u8]) -> Result<Vec<Key>, Error> {
    let raw = fx::unmarshal_byte_slices(b).map_err(Error::DecodeFileNode)?;
    let mut keys = Vec::with_capacity(raw.len());
    for (index, r) in raw.iter().enumerate() {
        let k = Key::parse(r).map_err(|source| Error::FileNodeChild { index, source })?;
        keys.push(k);
    }
    Ok(keys)
}

/// Decodes a DirLeaf body (a CBOR array of entry maps) into its entries, in
/// name order. The inverse of [`super::encode_dir_leaf`].
pub fn decode_dir_leaf(b: &[u8]) -> Result<Vec<Entry>, Error> {
    fx::unmarshal_entries(b).map_err(Error::DecodeDirLeaf)
}

/// Decodes a DirNode body (a CBOR array of `[sepName, childKey]` pairs) into
/// its pairs, in sepName order. The inverse of [`super::encode_dir_node`].
pub fn decode_dir_node(b: &[u8]) -> Result<Vec<DirPair>, Error> {
    fx::unmarshal_pairs(b).map_err(Error::DecodeDirNode)
}

// Every expectation below is the verbatim output of a Go oracle harness
// running fstree.Decode* / encMode.Marshal at the pinned commit (fxamacker
// cbor v2.9.2): the same inputs must produce the same decoded values, the
// same re-encoded bytes, and the same error strings.
#[cfg(test)]
mod tests {
    use super::super::encode::{encode_dir_leaf, marshal_entries, marshal_pairs};
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(s.len() % 2, 0, "odd hex length");
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
            .collect()
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// Hex of key.New(Blob, 100, "x"), the oracle's reference key.
    const KH: &str = "8d5497c63a49c025af4e80f66ba5856a09d40ab7ac02649a78f605d8e73a6400";

    #[test]
    fn file_node_ok_cases() {
        // (name, input hex, decoded key hexes)
        let cases: &[(&str, String, &[&str])] = &[
            ("ok", format!("825820{KH}5820{KH}"), &[KH, KH]),
            ("empty", "80".into(), &[]),
            ("null", "f6".into(), &[]),
            ("undefined", "f7".into(), &[]),
            ("indef_array", format!("9f5820{KH}ff"), &[KH]),
            (
                "nonshortest_arrayhead",
                format!("98025820{KH}5820{KH}"),
                &[KH, KH],
            ),
            ("top_tag_around_array", "d81880".into(), &[]),
        ];
        for (name, input, want) in cases {
            let got = decode_file_node(&unhex(input)).unwrap_or_else(|e| panic!("{name}: {e}"));
            let got: Vec<String> = got.iter().map(|k| k.to_string()).collect();
            assert_eq!(got, *want, "{name}");
        }
    }

    #[test]
    fn file_node_error_cases() {
        let deep33 = "81".repeat(33) + "40";
        let deep32 = "81".repeat(32) + "40";
        let cases: &[(&str, String, &str)] = &[
            (
                "nonshortest_bstrhead",
                "81580161".into(),
                "fstree: file node child 0: key: data is not 32 bytes: got 1",
            ),
            (
                "elem_null",
                "81f6".into(),
                "fstree: file node child 0: key: data is not 32 bytes: got 0",
            ),
            (
                "elem_tstr",
                "816161".into(),
                "fstree: decoding file node: cbor: cannot unmarshal UTF-8 text string into Go value of type []uint8",
            ),
            (
                "elem_uint",
                "8105".into(),
                "fstree: decoding file node: cbor: cannot unmarshal positive integer into Go value of type []uint8",
            ),
            (
                "elem_indef_bstr",
                "815f41ab41cdff".into(),
                "fstree: file node child 0: key: data is not 32 bytes: got 2",
            ),
            (
                "elem_short_bstr",
                "814161".into(),
                "fstree: file node child 0: key: data is not 32 bytes: got 1",
            ),
            (
                "trailing",
                "8000".into(),
                "fstree: decoding file node: cbor: 1 bytes of extraneous data starting at index 1",
            ),
            (
                "truncated",
                format!("825820{KH}"),
                "fstree: decoding file node: unexpected EOF",
            ),
            (
                "top_map",
                "a0".into(),
                "fstree: decoding file node: cbor: cannot unmarshal map into Go value of type [][]uint8",
            ),
            (
                "top_uint",
                "05".into(),
                "fstree: decoding file node: cbor: cannot unmarshal positive integer into Go value of type [][]uint8",
            ),
            (
                "top_bstr",
                "4100".into(),
                "fstree: decoding file node: cbor: cannot unmarshal byte string into Go value of type [][]uint8",
            ),
            (
                "elem_tagged_bstr",
                "81d8184100".into(),
                "fstree: file node child 0: key: data is not 32 bytes: got 1",
            ),
            (
                "garbage",
                "ff".into(),
                "fstree: decoding file node: cbor: unexpected \"break\" code",
            ),
            (
                "empty_input",
                String::new(),
                "fstree: decoding file node: EOF",
            ),
            (
                "elem_key_invalid",
                format!("815820 {} 08", &KH[..62]),
                "fstree: file node child 0: key: reserved header bit is set",
            ),
            (
                // The well-formedness pass runs first: extraneous data wins
                // over the element type error.
                "order_typeerr_plus_trailing",
                "810500".into(),
                "fstree: decoding file node: cbor: 1 bytes of extraneous data starting at index 2",
            ),
            (
                "order_utf8_elem",
                "8161ff".into(),
                "fstree: decoding file node: cbor: invalid UTF-8 string",
            ),
            (
                "elem_bool",
                "81f4".into(),
                "fstree: decoding file node: cbor: cannot unmarshal primitives into Go value of type []uint8",
            ),
            (
                "elem_float",
                "81f93c00".into(),
                "fstree: decoding file node: cbor: cannot unmarshal primitives into Go value of type []uint8",
            ),
            (
                "elem_array",
                "8180".into(),
                "fstree: file node child 0: key: data is not 32 bytes: got 0",
            ),
            (
                "elem_map",
                "81a0".into(),
                "fstree: decoding file node: cbor: cannot unmarshal map into Go value of type []uint8",
            ),
            (
                "top_bool",
                "f4".into(),
                "fstree: decoding file node: cbor: cannot unmarshal primitives into Go value of type [][]uint8",
            ),
            (
                "top_float",
                "f93c00".into(),
                "fstree: decoding file node: cbor: cannot unmarshal primitives into Go value of type [][]uint8",
            ),
            (
                "top_tstr",
                "6161".into(),
                "fstree: decoding file node: cbor: cannot unmarshal UTF-8 text string into Go value of type [][]uint8",
            ),
            (
                "elem_simple",
                "81f820".into(),
                "fstree: decoding file node: cbor: cannot unmarshal primitives into Go value of type []uint8",
            ),
            (
                "resv_ai_28",
                "811c".into(),
                "fstree: decoding file node: cbor: invalid additional information 28 for type positive integer",
            ),
            (
                "resv_ai_29",
                "811d".into(),
                "fstree: decoding file node: cbor: invalid additional information 29 for type positive integer",
            ),
            (
                "resv_ai_30",
                "811e".into(),
                "fstree: decoding file node: cbor: invalid additional information 30 for type positive integer",
            ),
            (
                "ai31_major0",
                "811f".into(),
                "fstree: decoding file node: cbor: invalid additional information 31 for type positive integer",
            ),
            (
                "ai31_major1",
                "813f".into(),
                "fstree: decoding file node: cbor: invalid additional information 31 for type negative integer",
            ),
            (
                "ai31_major6",
                "81df".into(),
                "fstree: decoding file node: cbor: invalid additional information 31 for type tag",
            ),
            (
                "indef_bstr_tstr_chunk",
                "815f6161ff".into(),
                "fstree: decoding file node: cbor: wrong element type UTF-8 text string for indefinite-length byte string",
            ),
            (
                "indef_bstr_nested_indef",
                "815f5fffff".into(),
                "fstree: decoding file node: cbor: indefinite-length byte string chunk is not definite-length",
            ),
            (
                "indef_bstr_nonstring_chunk",
                "815f00ff".into(),
                "fstree: decoding file node: cbor: wrong element type positive integer for indefinite-length byte string",
            ),
            (
                "depth_33",
                deep33,
                "fstree: decoding file node: cbor: exceeded max nested level 32",
            ),
            (
                "depth_32",
                deep32,
                "fstree: decoding file node: cbor: cannot unmarshal array into Go value of type uint8",
            ),
            (
                "array_count_131073",
                "9a00020001".into(),
                "fstree: decoding file node: cbor: exceeded max number of elements 131072 for CBOR array",
            ),
            (
                "array_count_131072_trunc",
                "9a00020000".into(),
                "fstree: decoding file node: unexpected EOF",
            ),
            (
                "bstr_len_u64max",
                "815bffffffffffffffff".into(),
                "fstree: decoding file node: cbor: byte string length 18446744073709551615 is too large, causing integer overflow",
            ),
            (
                "array_len_u64max",
                "9bffffffffffffffff".into(),
                "fstree: decoding file node: cbor: array length 18446744073709551615 is too large, it would cause integer overflow",
            ),
            (
                "map_len_u64max",
                "81bbffffffffffffffff".into(),
                "fstree: decoding file node: cbor: map length 18446744073709551615 is too large, it would cause integer overflow",
            ),
            (
                "tstr_len_u64max",
                "817bffffffffffffffff".into(),
                "fstree: decoding file node: cbor: UTF-8 text string length 18446744073709551615 is too large, causing integer overflow",
            ),
            (
                "bstr_len_i63",
                "815b7fffffffffffffff".into(),
                "fstree: decoding file node: unexpected EOF",
            ),
            (
                "bstr_len_2p63",
                "815b8000000000000000".into(),
                "fstree: decoding file node: cbor: byte string length 9223372036854775808 is too large, causing integer overflow",
            ),
            (
                "tag_eof",
                "81d8".into(),
                "fstree: decoding file node: unexpected EOF",
            ),
            (
                "tag_no_content",
                "81d818".into(),
                "fstree: decoding file node: unexpected EOF",
            ),
            (
                "wf_simple_24",
                "81f818".into(),
                "fstree: decoding file node: cbor: invalid simple value 24 for type primitives",
            ),
            (
                "wf_simple_31",
                "81f81f".into(),
                "fstree: decoding file node: cbor: invalid simple value 31 for type primitives",
            ),
            (
                "extraneous_idx",
                "800000".into(),
                "fstree: decoding file node: cbor: 2 bytes of extraneous data starting at index 1",
            ),
        ];
        for (name, input, want) in cases {
            let err = decode_file_node(&unhex(input)).expect_err(name);
            assert_eq!(err.to_string(), *want, "{name}");
        }
    }

    /// Expected outcome of a DirLeaf decode: the re-encoded bytes, an error,
    /// or a successful decode whose re-encode fails (non-canonical splice).
    enum Leaf {
        Reenc(&'static str),
        Fails(&'static str),
        ReencErr(&'static str),
    }

    #[test]
    fn dir_leaf_cases() {
        use Leaf::*;
        let cases: &[(&str, String, Leaf)] = &[
            (
                "ok_minimal",
                "81a50041610100020003000400".into(),
                Reenc("81a50041610100020003000400"),
            ),
            ("empty", "80".into(), Reenc("80")),
            ("null", "f6".into(), Reenc("80")),
            (
                "entry_empty_map",
                "81a0".into(),
                Reenc("81a500400100020003000400"),
            ),
            (
                "entry_null",
                "81f6".into(),
                Reenc("81a500400100020003000400"),
            ),
            (
                "unknown_int_key",
                "81a10a4161".into(),
                Reenc("81a500400100020003000400"),
            ),
            (
                "unknown_neg_key",
                "81a1204161".into(),
                Reenc("81a500400100020003000400"),
            ),
            (
                "unknown_tstr_key",
                "81a161304161".into(),
                Reenc("81a500400100020003000400"),
            ),
            // Duplicate key 0: the first value wins.
            (
                "dup_key_name",
                "81a2004161004162".into(),
                Reenc("81a50041610100020003000400"),
            ),
            (
                "mode_bstr",
                "81a1014161".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal byte string into Go struct field fstree.Entry.1 of type uint64",
                ),
            ),
            (
                "name_tstr",
                "81a1006161".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal UTF-8 text string into Go struct field fstree.Entry.0 of type []uint8",
                ),
            ),
            (
                "mtime_neg",
                "81a1043863".into(),
                Reenc("81a50040010002000300043863"),
            ),
            (
                "mtime_uint_overflow",
                "81a1041bffffffffffffffff".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal positive integer into Go struct field fstree.Entry.4 of type int64 (18446744073709551615 overflows int64)",
                ),
            ),
            (
                "mtime_neg_overflow",
                "81a1043bffffffffffffffff".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal negative integer into Go struct field fstree.Entry.4 of type int64 (-18446744073709551616 overflows Go's int64)",
                ),
            ),
            (
                "mtime_neg_min_ok",
                "81a1043b7fffffffffffffff".into(),
                Reenc("81a50040010002000300043b7fffffffffffffff"),
            ),
            (
                "mode_neg",
                "81a10120".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal negative integer into Go struct field fstree.Entry.1 of type uint64",
                ),
            ),
            (
                "rdev_ok",
                "81a107820103".into(),
                Reenc("81a60040010002000300040007820103"),
            ),
            (
                "rdev_neg_elem",
                "81a1078120".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal negative integer into Go struct field fstree.Entry.7 of type uint64",
                ),
            ),
            (
                "rdev_bstr",
                "81a1074161".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal byte string into Go struct field fstree.Entry.7 of type []uint64",
                ),
            ),
            (
                "xin_captures_map",
                "81a108a141614162".into(),
                Reenc("81a60040010002000300040008a141614162"),
            ),
            (
                "xin_captures_uint",
                "81a10805".into(),
                Reenc("81a6004001000200030004000805"),
            ),
            (
                // The decoder captures an indefinite-length RawMessage, but
                // the deterministic re-encode rejects it.
                "xin_captures_indef",
                "81a108bf41614162ff".into(),
                ReencErr(
                    "cbor: error calling MarshalCBOR for type cbor.RawMessage: cbor: indefinite-length map isn't allowed",
                ),
            ),
            (
                "xin_null",
                "81a108f6".into(),
                Reenc("81a60040010002000300040008f6"),
            ),
            (
                "xk_ok",
                format!("81a1095820{KH}"),
                Reenc(
                    "81a6004001000200030004000958208d5497c63a49c025af4e80f66ba5856a09d40ab7ac02649a78f605d8e73a6400",
                ),
            ),
            (
                "indef_map_entry",
                "81bf004161ff".into(),
                Reenc("81a50041610100020003000400"),
            ),
            (
                "indef_outer",
                "9fa1004161ff".into(),
                Reenc("81a50041610100020003000400"),
            ),
            (
                "trailing",
                "8000".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: 1 bytes of extraneous data starting at index 1",
                ),
            ),
            (
                "top_map",
                "a0".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal map into Go value of type []fstree.Entry",
                ),
            ),
            (
                "nonshortest_key",
                "81a118004161".into(),
                Reenc("81a50041610100020003000400"),
            ),
            (
                "float_mtime",
                "81a104f93c00".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal primitives into Go struct field fstree.Entry.4 of type int64",
                ),
            ),
            (
                "name_null",
                "81a100f6".into(),
                Reenc("81a500400100020003000400"),
            ),
            // Invalid UTF-8 in a skipped (unknown-key) value is tolerated…
            (
                "unknownkey_invalid_utf8_text",
                "81a10a61ff".into(),
                Reenc("81a500400100020003000400"),
            ),
            (
                "unknownkey_valid_utf8_text",
                "81a10a6161".into(),
                Reenc("81a500400100020003000400"),
            ),
            // …and duplicate-key values are skipped without type checking.
            (
                "dup_mode_second_bstr",
                "81a20100014161".into(),
                Reenc("81a500400100020003000400"),
            ),
            (
                "dup_name_second_uint",
                "81a2004161 0005".into(),
                Reenc("81a50041610100020003000400"),
            ),
            (
                "dup_key8_second",
                "81a208a00805".into(),
                Reenc("81a60040010002000300040008a0"),
            ),
            (
                "key_bool",
                "81a1f400".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal primitives into Go value of type string (map key is of type primitives and cannot be used to match struct field name)",
                ),
            ),
            (
                "key_maxuint",
                "81a11bffffffffffffffff00".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal positive integer into Go value of type int64 (18446744073709551615 overflows Go's int64)",
                ),
            ),
            (
                "key_neg_overflow",
                "81a13bffffffffffffffff00".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal negative integer into Go value of type int64 (-1-18446744073709551615 overflows Go's int64)",
                ),
            ),
            (
                "key_float",
                "81a1f93c0000".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal primitives into Go value of type string (map key is of type primitives and cannot be used to match struct field name)",
                ),
            ),
            (
                "key_bstr",
                "81a1413000".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal byte string into Go value of type string (map key is of type byte string and cannot be used to match struct field name)",
                ),
            ),
            (
                "key_array",
                "81a18000".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal array into Go value of type string (map key is of type array and cannot be used to match struct field name)",
                ),
            ),
            (
                "key_invalid_utf8_tstr",
                "81a161ff00".into(),
                Fails("fstree: decoding dir leaf: cbor: invalid UTF-8 string"),
            ),
            (
                "mode_null",
                "81a101f6".into(),
                Reenc("81a500400100020003000400"),
            ),
            (
                "mode_undefined",
                "81a101f7".into(),
                Reenc("81a500400100020003000400"),
            ),
            (
                "mode_bool",
                "81a101f4".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal primitives into Go struct field fstree.Entry.1 of type uint64",
                ),
            ),
            (
                "mode_float64",
                "81a101fb3ff0000000000000".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal primitives into Go struct field fstree.Entry.1 of type uint64",
                ),
            ),
            (
                "mode_float32",
                "81a101fa3f800000".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal primitives into Go struct field fstree.Entry.1 of type uint64",
                ),
            ),
            // Unassigned simple values decode as their number.
            (
                "mode_simple8",
                "81a101f820".into(),
                Reenc("81a50040011820020003000400"),
            ),
            (
                "mode_f0",
                "81a101f0".into(),
                Reenc("81a500400110020003000400"),
            ),
            (
                "mtime_f833",
                "81a104f833".into(),
                Reenc("81a50040010002000300041833"),
            ),
            (
                "mtime_bool",
                "81a104f5".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal primitives into Go struct field fstree.Entry.4 of type int64",
                ),
            ),
            (
                "ck_null",
                "81a105f6".into(),
                Reenc("81a500400100020003000400"),
            ),
            (
                "ck_tstr",
                "81a1056161".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal UTF-8 text string into Go struct field fstree.Entry.5 of type []uint8",
                ),
            ),
            (
                "rdev_null",
                "81a107f6".into(),
                Reenc("81a500400100020003000400"),
            ),
            (
                "rdev_elem_null",
                "81a10781f6".into(),
                Reenc("81a600400100020003000400078100"),
            ),
            (
                "rdev_elem_bool",
                "81a10781f4".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal primitives into Go struct field fstree.Entry.7 of type uint64",
                ),
            ),
            (
                "rdev_indef",
                "81a1079f0103ff".into(),
                Reenc("81a60040010002000300040007820103"),
            ),
            (
                "rdev_elem_maxuint",
                "81a107811bffffffffffffffff".into(),
                Reenc("81a60040010002000300040007811bffffffffffffffff"),
            ),
            (
                "rdev_elem_f0",
                "81a10781f0".into(),
                Reenc("81a600400100020003000400078110"),
            ),
            (
                "entry_uint",
                "8105".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal positive integer into Go value of type fstree.Entry",
                ),
            ),
            (
                "entry_array",
                "81854000000000".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal array into Go value of type fstree.Entry (cannot decode CBOR array to struct without toarray option)",
                ),
            ),
            (
                "entry_bstr",
                "814161".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal byte string into Go value of type fstree.Entry",
                ),
            ),
            (
                "entry_tagged_map",
                "81d823a1004161".into(),
                Reenc("81a50041610100020003000400"),
            ),
            (
                "top_tagged_array",
                "d82381a0".into(),
                Reenc("81a500400100020003000400"),
            ),
            (
                "mode_tagged_uint",
                "81a101d82305".into(),
                Reenc("81a500400105020003000400"),
            ),
            (
                "name_indef_bstr",
                "81a1005f41614162ff".into(),
                Reenc("81a5004261620100020003000400"),
            ),
            (
                "name_invalid_utf8_tstr_value",
                "81a10061ff".into(),
                Fails("fstree: decoding dir leaf: cbor: invalid UTF-8 string"),
            ),
            (
                "map_count_131073",
                "81ba00020001".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: exceeded max number of key-value pairs 131072 for CBOR map",
                ),
            ),
            // Text keys never match keyasint fields.
            (
                "tkey_1_val5",
                "81a1613105".into(),
                Reenc("81a500400100020003000400"),
            ),
            (
                "tkey_Mode_val5",
                "81a1644d6f646505".into(),
                Reenc("81a500400100020003000400"),
            ),
            (
                "tagged_key",
                "81a1d8230105".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal tag into Go value of type string (map key is of type tag and cannot be used to match struct field name)",
                ),
            ),
            (
                "name_f0",
                "81a100f0".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal primitives into Go struct field fstree.Entry.0 of type []uint8",
                ),
            ),
            (
                "entry_f0",
                "81f0".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal primitives into Go value of type fstree.Entry",
                ),
            ),
            (
                "top_f0",
                "f0".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal primitives into Go value of type []fstree.Entry",
                ),
            ),
            // Arrays decode element-wise into []byte, like Go reflection.
            (
                "name_arr_5_6",
                "81a100820506".into(),
                Reenc("81a5004205060100020003000400"),
            ),
            (
                "name_arr_256",
                "81a10081190100".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal positive integer into Go struct field fstree.Entry.0 of type uint8 (256 overflows uint8)",
                ),
            ),
            (
                "name_arr_neg",
                "81a1008120".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal negative integer into Go struct field fstree.Entry.0 of type uint8",
                ),
            ),
            (
                "name_arr_null",
                "81a10081f6".into(),
                Reenc("81a50041000100020003000400"),
            ),
            (
                "name_arr_nested",
                "81a1008180".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal array into Go struct field fstree.Entry.0 of type uint8",
                ),
            ),
            (
                "name_arr_bstr_elem",
                "81a100814105".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal byte string into Go struct field fstree.Entry.0 of type uint8",
                ),
            ),
            (
                "rdev_tstr",
                "81a1076161".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal UTF-8 text string into Go struct field fstree.Entry.7 of type []uint64",
                ),
            ),
            (
                "entry_tstr",
                "816161".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal UTF-8 text string into Go value of type fstree.Entry",
                ),
            ),
            (
                "entry_bool",
                "81f4".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal primitives into Go value of type fstree.Entry",
                ),
            ),
            (
                "top_bstr",
                "4161".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal byte string into Go value of type []fstree.Entry",
                ),
            ),
            (
                "indef_map_odd",
                "81bf00ff".into(),
                Fails("fstree: decoding dir leaf: cbor: unexpected \"break\" code"),
            ),
            (
                "mtime_uint_ok",
                "81a1041b7fffffffffffffff".into(),
                Reenc("81a50040010002000300041b7fffffffffffffff"),
            ),
            (
                "name_uint",
                "81a10005".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal positive integer into Go struct field fstree.Entry.0 of type []uint8",
                ),
            ),
            (
                "name_negint",
                "81a10020".into(),
                Fails(
                    "fstree: decoding dir leaf: cbor: cannot unmarshal negative integer into Go struct field fstree.Entry.0 of type []uint8",
                ),
            ),
            (
                "empty_input",
                String::new(),
                Fails("fstree: decoding dir leaf: EOF"),
            ),
        ];
        for (name, input, want) in cases {
            let got = decode_dir_leaf(&unhex(input));
            match (got, want) {
                (Ok(entries), Reenc(reenc)) => {
                    let b = marshal_entries(&entries)
                        .unwrap_or_else(|e| panic!("{name}: reencode: {e}"));
                    assert_eq!(
                        hex(&b),
                        unhex(reenc)
                            .iter()
                            .map(|x| format!("{x:02x}"))
                            .collect::<String>(),
                        "{name}: reencode"
                    );
                }
                (Ok(entries), ReencErr(msg)) => {
                    let err = marshal_entries(&entries).expect_err(name);
                    assert_eq!(err.to_string(), *msg, "{name}: reencode error");
                }
                (Err(e), Fails(msg)) => assert_eq!(e.to_string(), *msg, "{name}"),
                (Ok(_), Fails(msg)) => panic!("{name}: decoded OK, want error {msg}"),
                (Err(e), _) => panic!("{name}: unexpected error {e}"),
            }
        }
    }

    #[test]
    fn dir_leaf_decoded_values() {
        // Full value check of the richest oracle case.
        let entries = decode_dir_leaf(&unhex("81a50041610100020003000400")).unwrap();
        assert_eq!(
            entries,
            vec![Entry {
                name: b"a".to_vec(),
                ..Default::default()
            }]
        );
        let entries = decode_dir_leaf(&unhex("81a1043863")).unwrap();
        assert_eq!(entries[0].mtime, -100);
        let entries = decode_dir_leaf(&unhex("81a1043b7fffffffffffffff")).unwrap();
        assert_eq!(entries[0].mtime, i64::MIN);
        let entries = decode_dir_leaf(&unhex("81a107820103")).unwrap();
        assert_eq!(entries[0].rdev, vec![1, 3]);
        let entries = decode_dir_leaf(&unhex("81a108a141614162")).unwrap();
        assert_eq!(entries[0].xattrs_in, unhex("a141614162"));
        let entries = decode_dir_leaf(&unhex("81a101f820")).unwrap();
        assert_eq!(entries[0].mode, 32);
        // Duplicate name key: first wins.
        let entries = decode_dir_leaf(&unhex("81a2004161004162")).unwrap();
        assert_eq!(entries[0].name, b"a");
    }

    #[test]
    fn dir_leaf_round_trips_through_encode() {
        // decode → encode_dir_leaf reproduces canonical bytes and the key.
        let child = super::super::encode::encode_blob(&[7u8; 64]);
        let leaf = encode_dir_leaf(&[
            Entry {
                name: b"a".to_vec(),
                mode: 0o100644,
                uid: 1,
                gid: 2,
                mtime: -3,
                content_key: child.key.as_bytes().to_vec(),
                ..Default::default()
            },
            Entry {
                name: b"b".to_vec(),
                mode: 0o120777,
                link_target: b"a".to_vec(),
                ..Default::default()
            },
        ])
        .unwrap();
        let entries = decode_dir_leaf(&leaf.bytes).unwrap();
        let again = encode_dir_leaf(&entries).unwrap();
        assert_eq!(again.bytes, leaf.bytes);
        assert_eq!(again.key, leaf.key);
    }

    enum Node {
        Reenc(&'static str),
        Fails(&'static str),
    }

    #[test]
    fn dir_node_cases() {
        use Node::*;
        let cases: &[(&str, String, Node)] = &[
            (
                "ok",
                format!("8182 4161 5820{KH}"),
                Reenc(
                    "8182416158208d5497c63a49c025af4e80f66ba5856a09d40ab7ac02649a78f605d8e73a6400",
                ),
            ),
            ("empty", "80".into(), Reenc("80")),
            ("null", "f6".into(), Reenc("80")),
            (
                "pair_1elem",
                "81814161".into(),
                Fails(
                    "fstree: decoding dir node: cbor: cannot unmarshal array into Go value of type fstree.DirPair (cannot decode CBOR array to struct with different number of elements)",
                ),
            ),
            (
                "pair_3elem",
                "8183416141624163".into(),
                Fails(
                    "fstree: decoding dir node: cbor: cannot unmarshal array into Go value of type fstree.DirPair (cannot decode CBOR array to struct with different number of elements)",
                ),
            ),
            (
                "pair_empty_arr",
                "8180".into(),
                Fails(
                    "fstree: decoding dir node: cbor: cannot unmarshal array into Go value of type fstree.DirPair (cannot decode CBOR array to struct with different number of elements)",
                ),
            ),
            ("pair_null", "81f6".into(), Reenc("81824040")),
            (
                "pair_map",
                "81a0".into(),
                Fails(
                    "fstree: decoding dir node: cbor: cannot unmarshal map into Go value of type fstree.DirPair (cannot decode CBOR map to struct with toarray option)",
                ),
            ),
            (
                "pair_elem_tstr",
                format!("8182 6161 5820{KH}"),
                Fails(
                    "fstree: decoding dir node: cbor: cannot unmarshal UTF-8 text string into Go struct field fstree.DirPair.SepName of type []uint8",
                ),
            ),
            (
                "pair_elem_null",
                format!("8182 f6 5820{KH}"),
                Reenc("81824058208d5497c63a49c025af4e80f66ba5856a09d40ab7ac02649a78f605d8e73a6400"),
            ),
            (
                "indef_pair",
                format!("81 9f 4161 5820{KH} ff"),
                Reenc(
                    "8182416158208d5497c63a49c025af4e80f66ba5856a09d40ab7ac02649a78f605d8e73a6400",
                ),
            ),
            (
                "trailing",
                "8000".into(),
                Fails(
                    "fstree: decoding dir node: cbor: 1 bytes of extraneous data starting at index 1",
                ),
            ),
            (
                "pair_elem2_tstr",
                "818241616162".into(),
                Fails(
                    "fstree: decoding dir node: cbor: cannot unmarshal UTF-8 text string into Go struct field fstree.DirPair.ChildKey of type []uint8",
                ),
            ),
            (
                "pair_elem2_null_and_short",
                "81824161f6".into(),
                Reenc("8182416140"),
            ),
            (
                "pair_uint",
                "8105".into(),
                Fails(
                    "fstree: decoding dir node: cbor: cannot unmarshal positive integer into Go value of type fstree.DirPair",
                ),
            ),
            (
                "pair_bstr",
                "814161".into(),
                Fails(
                    "fstree: decoding dir node: cbor: cannot unmarshal byte string into Go value of type fstree.DirPair",
                ),
            ),
            (
                "pair_indef_1elem",
                "819f4161ff".into(),
                Fails(
                    "fstree: decoding dir node: cbor: cannot unmarshal array into Go value of type fstree.DirPair (cannot decode CBOR array to struct with different number of elements)",
                ),
            ),
            (
                "pair_indef_3elem",
                "819f416141624163ff".into(),
                Fails(
                    "fstree: decoding dir node: cbor: cannot unmarshal array into Go value of type fstree.DirPair (cannot decode CBOR array to struct with different number of elements)",
                ),
            ),
            (
                "top_uint",
                "05".into(),
                Fails(
                    "fstree: decoding dir node: cbor: cannot unmarshal positive integer into Go value of type []fstree.DirPair",
                ),
            ),
            (
                "top_bstr",
                "4161".into(),
                Fails(
                    "fstree: decoding dir node: cbor: cannot unmarshal byte string into Go value of type []fstree.DirPair",
                ),
            ),
            (
                "pair_elem2_tagged",
                "81824161d8184100".into(),
                Reenc("818241614100"),
            ),
            (
                "pair_elem2_undefined",
                "81824161f7".into(),
                Reenc("8182416140"),
            ),
            (
                "pair_f0",
                "81f0".into(),
                Fails(
                    "fstree: decoding dir node: cbor: cannot unmarshal primitives into Go value of type fstree.DirPair",
                ),
            ),
            (
                "empty_input",
                String::new(),
                Fails("fstree: decoding dir node: EOF"),
            ),
        ];
        for (name, input, want) in cases {
            let got = decode_dir_node(&unhex(input));
            match (got, want) {
                (Ok(pairs), Reenc(reenc)) => {
                    assert_eq!(
                        hex(&marshal_pairs(&pairs)),
                        hex(&unhex(reenc)),
                        "{name}: reencode"
                    );
                }
                (Err(e), Fails(msg)) => assert_eq!(e.to_string(), *msg, "{name}"),
                (Ok(_), Fails(msg)) => panic!("{name}: decoded OK, want error {msg}"),
                (Err(e), Reenc(_)) => panic!("{name}: unexpected error {e}"),
            }
        }
    }

    #[test]
    fn error_variants_are_matchable() {
        // Callers can match structurally, not just on strings.
        match decode_file_node(&[0x81, 0xf6]) {
            Err(Error::FileNodeChild { index: 0, source }) => {
                assert_eq!(source, crate::key::Error::BadKeyLength(0));
            }
            other => panic!("unexpected: {other:?}"),
        }
        match decode_file_node(&[]) {
            Err(Error::DecodeFileNode(super::super::CborError::Eof)) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }
}
