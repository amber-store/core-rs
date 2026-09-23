-- Reference records: name bytes -> canonical CBOR record bytes, verbatim.
-- Both columns are BLOBs, never NULL or TEXT, so names order by memcmp.
CREATE TABLE refs (
    name   BLOB NOT NULL PRIMARY KEY,
    record BLOB NOT NULL
) WITHOUT ROWID;
