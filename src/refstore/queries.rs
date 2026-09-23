//! The store's SQL statements (Go: `refstore/queries.sql`).
//!
//! Go generates its query code from `queries.sql` with sqlc; Rust has no
//! sqlc, so each statement is kept here as a constant holding the exact text
//! sqlc renders into `refstore/internal/refsdb/queries.sql.go` — the
//! `-- name:` line, the `?1`-style numbering of `sqlc.arg(...)` and the
//! trailing newline included — so both implementations send SQLite the same
//! statements. Names and records are always bound as BLOBs, never NULL or
//! TEXT: an empty record is an empty BLOB.

/// `GetRecord :one`.
pub(super) const GET_RECORD: &str = "-- name: GetRecord :one
SELECT record FROM refs WHERE name = ?
";

/// `PutRecord :exec`.
pub(super) const PUT_RECORD: &str = "-- name: PutRecord :exec
INSERT INTO refs (name, record) VALUES (?, ?)
ON CONFLICT (name) DO UPDATE SET record = excluded.record
";

/// `CreateRecord :execrows`.
pub(super) const CREATE_RECORD: &str = "-- name: CreateRecord :execrows
INSERT INTO refs (name, record) VALUES (?, ?)
ON CONFLICT (name) DO NOTHING
";

/// `ReplaceRecord :execrows`; parameters: record, name, old record.
pub(super) const REPLACE_RECORD: &str = "-- name: ReplaceRecord :execrows
UPDATE refs SET record = ?1
WHERE name = ?2 AND record = ?3
";

/// `DeleteRecord :execrows`.
pub(super) const DELETE_RECORD: &str = "-- name: DeleteRecord :execrows
DELETE FROM refs WHERE name = ?
";

/// `DeleteRecordIf :execrows`; parameters: name, old record.
pub(super) const DELETE_RECORD_IF: &str = "-- name: DeleteRecordIf :execrows
DELETE FROM refs WHERE name = ?1 AND record = ?2
";

/// `ListRecords :many`.
pub(super) const LIST_RECORDS: &str = "-- name: ListRecords :many
SELECT name, record FROM refs ORDER BY name
";

/// `DeleteAllRecords :exec`.
pub(super) const DELETE_ALL_RECORDS: &str = "-- name: DeleteAllRecords :exec
DELETE FROM refs
";
