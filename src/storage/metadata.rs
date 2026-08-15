use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct BucketRecord {
    pub name: String,
    pub created_at: String,
}

/// A resolved, current view of an object — the live version joined with its
/// single blob location. This is what the read/list paths consume.
#[derive(Debug, Clone)]
pub struct ObjectRecord {
    pub key: String,
    pub size: i64,
    pub etag: String,
    pub last_modified: String,
    pub content_type: Option<String>,
    pub account_email: String,
    pub remote_path: String,
    pub bucket_name: String,
    pub version: i64,
}

/// A row in the `versions` table (one version = one blob in the single-blob model).
#[derive(Debug, Clone)]
pub struct VersionRecord {
    pub bucket_name: String,
    pub key: String,
    pub version: i64,
    pub size: i64,
    pub etag: String,
    pub last_modified: String,
    pub content_type: Option<String>,
    pub account_email: String,
    pub remote_path: String,
    pub status: String,
    pub created_at: i64,
    pub superseded_at: Option<i64>,
}

/// Thread-safe metadata database wrapper.
/// All operations use blocking calls on a fresh connection per op.
#[derive(Clone)]
pub struct MetadataDb {
    path: String,
}

/// Build the pCloud remote path for a versioned blob (single-blob model).
///
/// Format: `{mount_prefix}/{bucket}/{key}.v{version}.c1`
///
/// The `.v{version}.c1` suffix is appended to the full key (keys may contain
/// `/` for nested paths). If the final path segment would exceed pCloud's
/// filename limit (~255 bytes), fall back to a content-hash name so the blob
/// stays addressable. The DB is authoritative for the key, so the pCloud name
/// is cosmetic.
pub fn build_remote_path(mount_prefix: &str, bucket: &str, key: &str, version: i64) -> String {
    let base = format!(
        "{}/{}/{}",
        mount_prefix.trim_end_matches('/'),
        bucket,
        key
    );
    let suffix = format!(".v{}.c1", version);
    const MAX_SEGMENT: usize = 200; // conservative margin under pCloud's limit

    let last_seg_len = base.rsplit('/').next().unwrap_or("").len();
    if last_seg_len + suffix.len() > MAX_SEGMENT {
        let digest = hex::encode(Sha256::digest(key.as_bytes()));
        let parent = base
            .rsplit_once('/')
            .map(|(p, _)| p.to_string())
            .unwrap_or_else(|| "/".to_string());
        return format!("{}/{}.v{}.c1", parent, &digest[..32], version);
    }

    format!("{}{}", base, suffix)
}

fn version_from_row(row: &rusqlite::Row) -> rusqlite::Result<VersionRecord> {
    Ok(VersionRecord {
        bucket_name: row.get(0)?,
        key: row.get(1)?,
        version: row.get(2)?,
        size: row.get(3)?,
        etag: row.get(4)?,
        last_modified: row.get(5)?,
        content_type: row.get(6)?,
        account_email: row.get(7)?,
        remote_path: row.get(8)?,
        status: row.get(9)?,
        created_at: row.get(10)?,
        superseded_at: row.get(11)?,
    })
}

fn object_from_row(row: &rusqlite::Row) -> rusqlite::Result<ObjectRecord> {
    Ok(ObjectRecord {
        key: row.get(0)?,
        size: row.get(1)?,
        etag: row.get(2)?,
        last_modified: row.get(3)?,
        content_type: row.get(4)?,
        account_email: row.get(5)?,
        remote_path: row.get(6)?,
        bucket_name: row.get(7)?,
        version: row.get(8)?,
    })
}

/// Column order shared by object-list queries (files JOIN versions).
const OBJECT_SELECT: &str = "v.key, v.size, v.etag, v.last_modified, v.content_type, \
     v.account_email, v.remote_path, v.bucket_name, v.version";

const VERSION_SELECT: &str = "bucket_name, key, version, size, etag, last_modified, \
     content_type, account_email, remote_path, status, created_at, superseded_at";

impl MetadataDb {
    fn table_exists(conn: &Connection, name: &str) -> anyhow::Result<bool> {
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![name],
            |row| row.get(0),
        )?;
        Ok(n > 0)
    }

    /// Run schema migrations from current version to latest.
    fn run_migrations(conn: &Connection) -> anyhow::Result<()> {
        // Ensure schema_version table exists
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (
                version INTEGER NOT NULL DEFAULT 0
            );",
        )?;

        // Get current version (defaults to 0 if no row exists)
        let current: i64 = conn
            .query_row(
                "SELECT COALESCE((SELECT version FROM schema_version LIMIT 1), 0)",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        // Ensure there is exactly one row in schema_version
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM schema_version", [], |row| row.get(0))?;
        if count == 0 {
            conn.execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                rusqlite::params![current],
            )?;
        }

        // Migration 1: add pcloud_account and pcloud_path to multipart_parts
        // (old schema had first_chunk/chunk_count; new has pcloud_account/pcloud_path)
        if current < 1 {
            let cols: Vec<String> = conn
                .prepare("PRAGMA table_info(multipart_parts)")?
                .query_map([], |row| row.get::<_, String>(1))?
                .filter_map(|r| r.ok())
                .collect();

            let has_first_chunk = cols.iter().any(|c| c == "first_chunk");

            if has_first_chunk {
                // Old schema with first_chunk/chunk_count (NOT NULL).
                conn.execute_batch(
                    "DROP TABLE IF EXISTS multipart_parts;
                     CREATE TABLE multipart_parts (
                        upload_id TEXT NOT NULL,
                        part_number INTEGER NOT NULL,
                        size INTEGER NOT NULL,
                        part_etag TEXT NOT NULL,
                        pcloud_account TEXT NOT NULL,
                        pcloud_path TEXT NOT NULL,
                        PRIMARY KEY (upload_id, part_number)
                     );",
                )?;
            } else {
                if !cols.iter().any(|c| c == "pcloud_account") {
                    conn.execute_batch(
                        "ALTER TABLE multipart_parts ADD COLUMN pcloud_account TEXT NOT NULL DEFAULT '';",
                    )?;
                }
                if !cols.iter().any(|c| c == "pcloud_path") {
                    conn.execute_batch(
                        "ALTER TABLE multipart_parts ADD COLUMN pcloud_path TEXT NOT NULL DEFAULT '';",
                    )?;
                }
            }

            conn.execute("UPDATE schema_version SET version = 1", [])?;
        }

        // Migration 2: introduce `files` + `versions` (MVCC overwrite) and copy
        // any legacy `objects` rows into them as version 1. Pure metadata
        // migration — no blob is moved, renamed, or deleted.
        if current < 2 {
            // The pre-MVCC (v0.1.x) schema stored logical objects in `objects`
            // and physical chunked data in `files` + `chunks`. The new schema
            // uses `files` (object index) + `versions` (immutable versions) and
            // stores every object as a single blob. If an old `files` table is
            // still present (no `current_version` column), preserve it as
            // `files_legacy` before creating the new schema.
            if Self::table_exists(conn, "files")? {
                let file_cols: Vec<String> = conn
                    .prepare("PRAGMA table_info(files)")?
                    .query_map([], |row| row.get::<_, String>(1))?
                    .filter_map(|r| r.ok())
                    .collect();
                if !file_cols.iter().any(|c| c == "current_version") {
                    conn.execute_batch("ALTER TABLE files RENAME TO files_legacy;")?;
                }
            }

            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS files (
                    bucket_name      TEXT NOT NULL,
                    key              TEXT NOT NULL,
                    current_version  INTEGER NOT NULL,
                    size             INTEGER NOT NULL DEFAULT 0,
                    etag             TEXT NOT NULL,
                    last_modified    TEXT NOT NULL,
                    content_type     TEXT,
                    PRIMARY KEY (bucket_name, key)
                );

                CREATE TABLE IF NOT EXISTS versions (
                    bucket_name      TEXT NOT NULL,
                    key              TEXT NOT NULL,
                    version          INTEGER NOT NULL,
                    size             INTEGER NOT NULL DEFAULT 0,
                    etag             TEXT NOT NULL,
                    last_modified    TEXT NOT NULL,
                    content_type     TEXT,
                    account_email    TEXT NOT NULL,
                    remote_path      TEXT NOT NULL,
                    status           TEXT NOT NULL,
                    created_at       INTEGER NOT NULL,
                    superseded_at    INTEGER,
                    PRIMARY KEY (bucket_name, key, version)
                );",
            )?;

            // Re-point the bucket index at the new `files` table (it may have
            // been created on the old schema before the rename above).
            conn.execute_batch(
                "DROP INDEX IF EXISTS idx_files_bucket;
                 CREATE INDEX IF NOT EXISTS idx_files_bucket ON files(bucket_name);",
            )?;

            if Self::table_exists(conn, "objects")? {
                conn.execute_batch("BEGIN;")?;
                let copy = (|| -> anyhow::Result<()> {
                    conn.execute(
                        "INSERT INTO files (bucket_name, key, current_version, size, etag, last_modified, content_type)
                         SELECT bucket_name, key, 1, size, etag, last_modified, content_type FROM objects",
                        [],
                    )?;
                    conn.execute(
                        "INSERT INTO versions (bucket_name, key, version, size, etag, last_modified, content_type, account_email, remote_path, status, created_at, superseded_at)
                         SELECT bucket_name, key, 1, size, etag, last_modified, content_type, account_email, remote_path, 'committed', CAST(strftime('%s','now') AS INTEGER) * 1000, NULL FROM objects",
                        [],
                    )?;
                    Ok(())
                })();
                match copy {
                    Ok(()) => conn.execute_batch("COMMIT;")?,
                    Err(e) => {
                        let _ = conn.execute_batch("ROLLBACK;");
                        return Err(e);
                    }
                }
                // Keep legacy data available (reversible) rather than dropping it.
                conn.execute_batch("ALTER TABLE objects RENAME TO objects_legacy;")?;
            }

            conn.execute("UPDATE schema_version SET version = 2", [])?;
        }

        // Migration 3: add a `checksum` column (SHA-256 of blob content) to
        // `versions` and `files`. Used to detect accidental in-place
        // modification of a managed blob. Populated lazily — existing rows get
        // an empty checksum until `multifs checksum rebuild` computes it.
        if current < 3 {
            let vcols: Vec<String> = conn
                .prepare("PRAGMA table_info(versions)")?
                .query_map([], |row| row.get::<_, String>(1))?
                .filter_map(|r| r.ok())
                .collect();
            if !vcols.iter().any(|c| c == "checksum") {
                conn.execute_batch(
                    "ALTER TABLE versions ADD COLUMN checksum TEXT NOT NULL DEFAULT '';",
                )?;
            }

            let fcols: Vec<String> = conn
                .prepare("PRAGMA table_info(files)")?
                .query_map([], |row| row.get::<_, String>(1))?
                .filter_map(|r| r.ok())
                .collect();
            if !fcols.iter().any(|c| c == "checksum") {
                conn.execute_batch(
                    "ALTER TABLE files ADD COLUMN checksum TEXT NOT NULL DEFAULT '';",
                )?;
            }

            conn.execute("UPDATE schema_version SET version = 3", [])?;
        }

        Ok(())
    }

    pub fn open(path: &str) -> anyhow::Result<Self> {
        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS buckets (
                name TEXT PRIMARY KEY,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );

            CREATE TABLE IF NOT EXISTS files (
                bucket_name      TEXT NOT NULL,
                key              TEXT NOT NULL,
                current_version  INTEGER NOT NULL,
                size             INTEGER NOT NULL DEFAULT 0,
                etag             TEXT NOT NULL,
                last_modified    TEXT NOT NULL,
                content_type     TEXT,
                checksum         TEXT NOT NULL DEFAULT '',
                PRIMARY KEY (bucket_name, key)
            );

            CREATE TABLE IF NOT EXISTS versions (
                bucket_name      TEXT NOT NULL,
                key              TEXT NOT NULL,
                version          INTEGER NOT NULL,
                size             INTEGER NOT NULL DEFAULT 0,
                etag             TEXT NOT NULL,
                last_modified    TEXT NOT NULL,
                content_type     TEXT,
                account_email    TEXT NOT NULL,
                remote_path      TEXT NOT NULL,
                status           TEXT NOT NULL,
                created_at       INTEGER NOT NULL,
                superseded_at    INTEGER,
                checksum         TEXT NOT NULL DEFAULT '',
                PRIMARY KEY (bucket_name, key, version)
            );

            CREATE INDEX IF NOT EXISTS idx_files_bucket ON files(bucket_name);
            CREATE INDEX IF NOT EXISTS idx_versions_account ON versions(account_email);
            CREATE INDEX IF NOT EXISTS idx_versions_superseded ON versions(status, superseded_at);

            CREATE TABLE IF NOT EXISTS multipart_uploads (
                upload_id TEXT PRIMARY KEY,
                bucket TEXT NOT NULL,
                key TEXT NOT NULL,
                content_type TEXT,
                created INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS multipart_parts (
                upload_id TEXT NOT NULL,
                part_number INTEGER NOT NULL,
                size INTEGER NOT NULL,
                part_etag TEXT NOT NULL,
                pcloud_account TEXT NOT NULL,
                pcloud_path TEXT NOT NULL,
                PRIMARY KEY (upload_id, part_number)
            );
            ",
        )?;

        // Run schema migrations to upgrade old databases
        Self::run_migrations(&conn)?;

        Ok(Self {
            path: path.to_string(),
        })
    }

    pub fn with_conn<F, T>(&self, f: F) -> anyhow::Result<T>
    where
        F: FnOnce(&Connection) -> anyhow::Result<T>,
    {
        let conn = Connection::open(&self.path)?;
        conn.busy_timeout(Duration::from_millis(5000))?;
        f(&conn)
    }

    // ---- Bucket operations ----

    pub fn bucket_exists(&self, name: &str) -> anyhow::Result<bool> {
        self.with_conn(|conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM buckets WHERE name = ?1",
                params![name],
                |row| row.get(0),
            )?;
            Ok(count > 0)
        })
    }

    pub fn create_bucket(&self, name: &str) -> anyhow::Result<()> {
        self.with_conn(|conn| {
            conn.execute("INSERT INTO buckets (name) VALUES (?1)", params![name])?;
            Ok(())
        })
    }

    pub fn delete_bucket(&self, name: &str) -> anyhow::Result<()> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM buckets WHERE name = ?1", params![name])?;
            Ok(())
        })
    }

    pub fn get_bucket(&self, name: &str) -> anyhow::Result<Option<BucketRecord>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT name, created_at FROM buckets WHERE name = ?1")?;
            let mut rows = stmt.query(params![name])?;
            if let Some(row) = rows.next()? {
                Ok(Some(BucketRecord {
                    name: row.get(0)?,
                    created_at: row.get(1)?,
                }))
            } else {
                Ok(None)
            }
        })
    }

    pub fn list_buckets(&self) -> anyhow::Result<Vec<BucketRecord>> {
        self.with_conn(|conn| {
            let mut stmt =
                conn.prepare("SELECT name, created_at FROM buckets ORDER BY name")?;
            let rows = stmt.query_map([], |row| {
                Ok(BucketRecord {
                    name: row.get(0)?,
                    created_at: row.get(1)?,
                })
            })?;
            let mut buckets = Vec::new();
            for row in rows {
                buckets.push(row?);
            }
            Ok(buckets)
        })
    }

    // ---- Versioned object operations ----

    /// Reserve the next version number for `(bucket, key)` and insert a
    /// `pending` version row. Returns `(version, remote_path)`.
    ///
    /// The version is allocated atomically (Postgres-xid style) so concurrent
    /// writers to the same key each get a distinct number; aborted uploads
    /// simply leave a gap in the numbering.
    pub fn reserve_version(
        &self,
        bucket: &str,
        key: &str,
        account_email: &str,
        mount_prefix: &str,
    ) -> anyhow::Result<(i64, String)> {
        self.with_conn(|conn| {
            conn.execute_batch("BEGIN IMMEDIATE;")?;
            let result = (|| -> anyhow::Result<(i64, String)> {
                let version: i64 = conn.query_row(
                    "SELECT COALESCE(MAX(version), 0) + 1 FROM versions WHERE bucket_name = ?1 AND key = ?2",
                    params![bucket, key],
                    |row| row.get(0),
                )?;
                let remote_path = build_remote_path(mount_prefix, bucket, key, version);
                let now = chrono::Utc::now().timestamp_millis();
                conn.execute(
                    "INSERT INTO versions (bucket_name, key, version, size, etag, last_modified, content_type, account_email, remote_path, status, created_at, superseded_at)
                     VALUES (?1, ?2, ?3, 0, '', '', NULL, ?4, ?5, 'pending', ?6, NULL)",
                    params![bucket, key, version, account_email, remote_path, now],
                )?;
                Ok((version, remote_path))
            })();
            match result {
                Ok(v) => {
                    conn.execute_batch("COMMIT;")?;
                    Ok(v)
                }
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK;");
                    Err(e)
                }
            }
        })
    }

    /// Commit a reserved version: mark it `committed`, flip the file's current
    /// pointer, and supersede the previous current version — all in one
    /// transaction (the atomic commit point).
    #[allow(clippy::too_many_arguments)]
    pub fn commit_version(
        &self,
        bucket: &str,
        key: &str,
        version: i64,
        size: i64,
        etag: &str,
        last_modified: &str,
        content_type: Option<&str>,
        remote_path: &str,
    ) -> anyhow::Result<()> {
        self.with_conn(|conn| {
            conn.execute_batch("BEGIN IMMEDIATE;")?;
            let result = (|| -> anyhow::Result<()> {
                let now = chrono::Utc::now().timestamp_millis();

                // Supersede the previous current version (if any, and not ours).
                let prev: Option<i64> = conn
                    .query_row(
                        "SELECT current_version FROM files WHERE bucket_name = ?1 AND key = ?2",
                        params![bucket, key],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(p) = prev {
                    if p != version {
                        conn.execute(
                            "UPDATE versions SET superseded_at = ?1
                             WHERE bucket_name = ?2 AND key = ?3 AND version = ?4 AND superseded_at IS NULL",
                            params![now, bucket, key, p],
                        )?;
                    }
                }

                // Mark the new version committed.
                conn.execute(
                    "UPDATE versions SET status = 'committed', size = ?1, etag = ?2, last_modified = ?3, content_type = ?4, remote_path = ?5
                     WHERE bucket_name = ?6 AND key = ?7 AND version = ?8",
                    params![size, etag, last_modified, content_type, remote_path, bucket, key, version],
                )?;

                // Upsert the file pointer.
                conn.execute(
                    "INSERT INTO files (bucket_name, key, current_version, size, etag, last_modified, content_type)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(bucket_name, key) DO UPDATE SET
                       current_version = excluded.current_version,
                       size = excluded.size,
                       etag = excluded.etag,
                       last_modified = excluded.last_modified,
                       content_type = excluded.content_type",
                    params![bucket, key, version, size, etag, last_modified, content_type],
                )?;

                Ok(())
            })();
            match result {
                Ok(()) => {
                    conn.execute_batch("COMMIT;")?;
                    Ok(())
                }
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK;");
                    Err(e)
                }
            }
        })
    }

    /// Adopt an existing remote file into the object index without moving data.
    ///
    /// Creates a committed version row + file pointer in a single transaction,
    /// pointing at the existing `remote_path`. Used to register files that were
    /// uploaded to pCloud outside of multifs (e.g. by the video-subtitle pipeline).
    #[allow(clippy::too_many_arguments)]
    pub fn import_object(
        &self,
        bucket: &str,
        key: &str,
        account_email: &str,
        remote_path: &str,
        size: i64,
        etag: &str,
        last_modified: &str,
        content_type: Option<&str>,
    ) -> anyhow::Result<()> {
        self.with_conn(|conn| {
            conn.execute_batch("BEGIN IMMEDIATE;")?;
            let result = (|| -> anyhow::Result<()> {
                let now = chrono::Utc::now().timestamp_millis();
                let version: i64 = conn.query_row(
                    "SELECT COALESCE(MAX(version), 0) + 1 FROM versions WHERE bucket_name = ?1 AND key = ?2",
                    params![bucket, key],
                    |row| row.get(0),
                )?;
                conn.execute(
                    "INSERT INTO versions (bucket_name, key, version, size, etag, last_modified, content_type, account_email, remote_path, status, created_at, superseded_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'committed', ?10, NULL)",
                    params![bucket, key, version, size, etag, last_modified, content_type, account_email, remote_path, now],
                )?;
                conn.execute(
                    "INSERT INTO files (bucket_name, key, current_version, size, etag, last_modified, content_type)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(bucket_name, key) DO UPDATE SET
                       current_version = excluded.current_version,
                       size = excluded.size,
                       etag = excluded.etag,
                       last_modified = excluded.last_modified,
                       content_type = excluded.content_type",
                    params![bucket, key, version, size, etag, last_modified, content_type],
                )?;
                Ok(())
            })();
            match result {
                Ok(()) => {
                    conn.execute_batch("COMMIT;")?;
                    Ok(())
                }
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK;");
                    Err(e)
                }
            }
        })
    }

    /// Return the (bucket, key) under which `remote_path` is already tracked for
    /// `account_email`, if any (committed versions only).
    /// Store the SHA-256 checksum for a version's blob. Mirrors the value onto
    /// the `files` row when `version` is that file's current version, so a
    /// `get_object` can surface the checksum without a second lookup.
    pub fn set_checksum(
        &self,
        bucket: &str,
        key: &str,
        version: i64,
        checksum: &str,
    ) -> anyhow::Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE versions SET checksum = ?4 WHERE bucket_name = ?1 AND key = ?2 AND version = ?3",
                params![bucket, key, version, checksum],
            )?;
            conn.execute(
                "UPDATE files SET checksum = ?3 WHERE bucket_name = ?1 AND key = ?2 AND current_version = ?4",
                params![bucket, key, checksum, version],
            )?;
            Ok(())
        })
    }

    /// Return the stored checksum for a file's current version, if any.
    pub fn get_checksum(&self, bucket: &str, key: &str) -> anyhow::Result<Option<String>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT checksum FROM files WHERE bucket_name = ?1 AND key = ?2",
                    params![bucket, key],
                    |row| row.get(0),
                )
                .optional()?)
        })
    }

    pub fn find_object_by_remote_path(
        &self,
        account_email: &str,
        remote_path: &str,
    ) -> anyhow::Result<Option<(String, String)>> {
        self.with_conn(|conn| {
            let found: Option<(String, String)> = conn
                .query_row(
                    "SELECT bucket_name, key FROM versions
                     WHERE account_email = ?1 AND remote_path = ?2 AND status = 'committed'
                     ORDER BY created_at DESC LIMIT 1",
                    params![account_email, remote_path],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            Ok(found)
        })
    }

    pub fn get_object(&self, bucket: &str, key: &str) -> anyhow::Result<Option<ObjectRecord>> {
        self.with_conn(|conn| {
            let sql = format!(
                "SELECT {} FROM files f \
                 JOIN versions v ON v.bucket_name = f.bucket_name AND v.key = f.key AND v.version = f.current_version \
                 WHERE f.bucket_name = ?1 AND f.key = ?2",
                OBJECT_SELECT
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut rows = stmt.query(params![bucket, key])?;
            if let Some(row) = rows.next()? {
                Ok(Some(object_from_row(row)?))
            } else {
                Ok(None)
            }
        })
    }

    pub fn delete_object(&self, bucket: &str, key: &str) -> anyhow::Result<()> {
        self.with_conn(|conn| {
            conn.execute_batch("BEGIN IMMEDIATE;")?;
            let result = (|| -> anyhow::Result<()> {
                let now = chrono::Utc::now().timestamp_millis();
                // Mark the current version superseded so vacuum reclaims its blob.
                conn.execute(
                    "UPDATE versions SET superseded_at = ?1
                     WHERE bucket_name = ?2 AND key = ?3
                       AND version = (SELECT current_version FROM files WHERE bucket_name = ?2 AND key = ?3)
                       AND superseded_at IS NULL",
                    params![now, bucket, key],
                )?;
                conn.execute(
                    "DELETE FROM files WHERE bucket_name = ?1 AND key = ?2",
                    params![bucket, key],
                )?;
                Ok(())
            })();
            match result {
                Ok(()) => {
                    conn.execute_batch("COMMIT;")?;
                    Ok(())
                }
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK;");
                    Err(e)
                }
            }
        })
    }

    /// List objects in a bucket with optional prefix, start-after, and limit.
    /// Over-fetches by 1 if the caller wants to detect truncation.
    pub fn list_objects(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        start_after: Option<&str>,
        max_keys: i64,
    ) -> anyhow::Result<Vec<ObjectRecord>> {
        self.with_conn(|conn| {
            let (sql, param_vec): (&str, Vec<Box<dyn rusqlite::types::ToSql>>) = match (
                prefix,
                start_after,
            ) {
                (Some(p), Some(sa)) => {
                    let pattern = format!("{}%", p);
                    (
                        "SELECT v.key, v.size, v.etag, v.last_modified, v.content_type, \
                         v.account_email, v.remote_path, v.bucket_name, v.version \
                         FROM files f JOIN versions v ON v.bucket_name = f.bucket_name AND v.key = f.key AND v.version = f.current_version \
                         WHERE f.bucket_name = ?1 AND f.key LIKE ?2 AND f.key > ?3 \
                         ORDER BY f.key LIMIT ?4",
                        vec![
                            Box::new(bucket.to_string()),
                            Box::new(pattern),
                            Box::new(sa.to_string()),
                            Box::new(max_keys),
                        ],
                    )
                }
                (Some(p), None) => {
                    let pattern = format!("{}%", p);
                    (
                        "SELECT v.key, v.size, v.etag, v.last_modified, v.content_type, \
                         v.account_email, v.remote_path, v.bucket_name, v.version \
                         FROM files f JOIN versions v ON v.bucket_name = f.bucket_name AND v.key = f.key AND v.version = f.current_version \
                         WHERE f.bucket_name = ?1 AND f.key LIKE ?2 \
                         ORDER BY f.key LIMIT ?3",
                        vec![
                            Box::new(bucket.to_string()),
                            Box::new(pattern),
                            Box::new(max_keys),
                        ],
                    )
                }
                (None, Some(sa)) => (
                    "SELECT v.key, v.size, v.etag, v.last_modified, v.content_type, \
                     v.account_email, v.remote_path, v.bucket_name, v.version \
                     FROM files f JOIN versions v ON v.bucket_name = f.bucket_name AND v.key = f.key AND v.version = f.current_version \
                     WHERE f.bucket_name = ?1 AND f.key > ?2 \
                     ORDER BY f.key LIMIT ?3",
                    vec![
                        Box::new(bucket.to_string()),
                        Box::new(sa.to_string()),
                        Box::new(max_keys),
                    ],
                ),
                (None, None) => (
                    "SELECT v.key, v.size, v.etag, v.last_modified, v.content_type, \
                     v.account_email, v.remote_path, v.bucket_name, v.version \
                     FROM files f JOIN versions v ON v.bucket_name = f.bucket_name AND v.key = f.key AND v.version = f.current_version \
                     WHERE f.bucket_name = ?1 \
                     ORDER BY f.key LIMIT ?2",
                    vec![Box::new(bucket.to_string()), Box::new(max_keys)],
                ),
            };
            let mut stmt = conn.prepare(sql)?;
            let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                param_vec.iter().map(|p| p.as_ref()).collect();
            let rows = stmt.query_map(param_refs.as_slice(), object_from_row)?;
            let mut objects = Vec::new();
            for row in rows {
                objects.push(row?);
            }
            Ok(objects)
        })
    }

    pub fn list_all_objects(&self) -> anyhow::Result<Vec<ObjectRecord>> {
        self.with_conn(|conn| {
            let sql = format!(
                "SELECT {} FROM files f \
                 JOIN versions v ON v.bucket_name = f.bucket_name AND v.key = f.key AND v.version = f.current_version \
                 ORDER BY f.bucket_name, f.key",
                OBJECT_SELECT
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map([], object_from_row)?;
            let mut objects = Vec::new();
            for row in rows {
                objects.push(row?);
            }
            Ok(objects)
        })
    }

    pub fn delete_all_objects(&self, bucket: &str) -> anyhow::Result<()> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM files WHERE bucket_name = ?1", params![bucket])?;
            Ok(())
        })
    }

    pub fn count_objects(&self, bucket: &str) -> anyhow::Result<i64> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM files WHERE bucket_name = ?1",
                params![bucket],
                |row| row.get(0),
            )
            .map_err(anyhow::Error::from)
        })
    }

    pub fn bucket_total_size(&self, bucket: &str) -> anyhow::Result<i64> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT COALESCE(SUM(size), 0) FROM files WHERE bucket_name = ?1",
                params![bucket],
                |row| row.get(0),
            )
            .map_err(anyhow::Error::from)
        })
    }

    pub fn count_objects_for_account(&self, email: &str) -> anyhow::Result<i64> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM files f JOIN versions v ON v.bucket_name = f.bucket_name AND v.key = f.key AND v.version = f.current_version \
                 WHERE v.account_email = ?1",
                params![email],
                |row| row.get(0),
            )
            .map_err(anyhow::Error::from)
        })
    }

    pub fn account_total_size(&self, email: &str) -> anyhow::Result<i64> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT COALESCE(SUM(v.size), 0) FROM files f JOIN versions v ON v.bucket_name = f.bucket_name AND v.key = f.key AND v.version = f.current_version \
                 WHERE v.account_email = ?1",
                params![email],
                |row| row.get(0),
            )
            .map_err(anyhow::Error::from)
        })
    }

    // ---- Garbage collection (vacuum) ----

    /// Pending versions older than `cutoff_ms` (failed/abandoned uploads).
    pub fn list_pending_versions(&self, cutoff_ms: i64) -> anyhow::Result<Vec<VersionRecord>> {
        self.with_conn(|conn| {
            let sql = format!(
                "SELECT {} FROM versions WHERE status = 'pending' AND created_at < ?1",
                VERSION_SELECT
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params![cutoff_ms], version_from_row)?;
            let mut v = Vec::new();
            for r in rows {
                v.push(r?);
            }
            Ok(v)
        })
    }

    /// Committed versions superseded before `cutoff_ms` (safe to reclaim).
    pub fn list_orphan_versions(&self, cutoff_ms: i64) -> anyhow::Result<Vec<VersionRecord>> {
        self.with_conn(|conn| {
            let sql = format!(
                "SELECT {} FROM versions WHERE status = 'committed' AND superseded_at IS NOT NULL AND superseded_at < ?1",
                VERSION_SELECT
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params![cutoff_ms], version_from_row)?;
            let mut v = Vec::new();
            for r in rows {
                v.push(r?);
            }
            Ok(v)
        })
    }

    /// Remove a version row (call after its blob has been deleted from pCloud).
    pub fn delete_version(&self, bucket: &str, key: &str, version: i64) -> anyhow::Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "DELETE FROM versions WHERE bucket_name = ?1 AND key = ?2 AND version = ?3",
                params![bucket, key, version],
            )?;
            Ok(())
        })
    }

    // ---- Multipart upload helpers (simplified — one blob per part) ----

    pub fn create_multipart(
        &self,
        upload_id: &str,
        bucket: &str,
        key: &str,
        content_type: Option<&str>,
    ) -> anyhow::Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO multipart_uploads \
                 (upload_id, bucket, key, content_type, created) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    upload_id,
                    bucket,
                    key,
                    content_type,
                    chrono::Utc::now().timestamp()
                ],
            )?;
            Ok(())
        })
    }

    pub fn get_multipart(
        &self,
        upload_id: &str,
    ) -> anyhow::Result<Option<(String, String, Option<String>)>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT bucket, key, content_type FROM multipart_uploads WHERE upload_id = ?1",
            )?;
            let mut rows = stmt.query(params![upload_id])?;
            if let Some(row) = rows.next()? {
                Ok(Some((row.get(0)?, row.get(1)?, row.get(2)?)))
            } else {
                Ok(None)
            }
        })
    }

    pub fn delete_multipart(&self, upload_id: &str) -> anyhow::Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "DELETE FROM multipart_uploads WHERE upload_id = ?1",
                params![upload_id],
            )?;
            conn.execute(
                "DELETE FROM multipart_parts WHERE upload_id = ?1",
                params![upload_id],
            )?;
            Ok(())
        })
    }

    /// Remove only the transient multipart_uploads row, KEEPING the multipart_parts
    /// rows so a completed object can still be assembled on read.
    pub fn delete_multipart_upload(&self, upload_id: &str) -> anyhow::Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "DELETE FROM multipart_uploads WHERE upload_id = ?1",
                params![upload_id],
            )?;
            Ok(())
        })
    }

    pub fn store_multipart_part(
        &self,
        upload_id: &str,
        part_number: u64,
        size: i64,
        part_etag: &str,
        pcloud_account: &str,
        pcloud_path: &str,
    ) -> anyhow::Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO multipart_parts \
                 (upload_id, part_number, size, part_etag, pcloud_account, pcloud_path) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    upload_id,
                    part_number as i64,
                    size,
                    part_etag,
                    pcloud_account,
                    pcloud_path
                ],
            )?;
            Ok(())
        })
    }

    pub fn list_multipart_parts(
        &self,
        upload_id: &str,
    ) -> anyhow::Result<Vec<(u64, i64, String, String, String)>> {
        // Returns: (part_number, size, part_etag, pcloud_account, pcloud_path)
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT part_number, size, part_etag, pcloud_account, pcloud_path \
                 FROM multipart_parts \
                 WHERE upload_id = ?1 ORDER BY part_number",
            )?;
            let rows = stmt.query_map(params![upload_id], |row| {
                Ok((
                    row.get::<_, i64>(0)? as u64,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })?;
            let mut v = Vec::new();
            for r in rows {
                v.push(r?);
            }
            Ok(v)
        })
    }

    /// In-progress multipart uploads initiated before `cutoff_sec` (epoch
    /// seconds) that were never completed — safe to reclaim. Returns upload_ids.
    pub fn list_abandoned_multipart_uploads(&self, cutoff_sec: i64) -> anyhow::Result<Vec<String>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT upload_id FROM multipart_uploads WHERE created < ?1",
            )?;
            let rows = stmt.query_map(params![cutoff_sec], |row| row.get::<_, String>(0))?;
            let mut v = Vec::new();
            for r in rows {
                v.push(r?);
            }
            Ok(v)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// Create a database with the OLD schema (objects + old multipart_parts) to
    /// simulate an existing installation from before the MVCC migration.
    fn create_old_schema(path: &str) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE buckets (
                name TEXT PRIMARY KEY,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
             );
             CREATE TABLE objects (
                bucket_name TEXT NOT NULL,
                key TEXT NOT NULL,
                size INTEGER NOT NULL DEFAULT 0,
                etag TEXT NOT NULL,
                last_modified TEXT NOT NULL,
                content_type TEXT,
                account_email TEXT NOT NULL,
                remote_path TEXT NOT NULL,
                PRIMARY KEY (bucket_name, key)
             );
             CREATE TABLE multipart_uploads (
                upload_id TEXT PRIMARY KEY,
                bucket TEXT NOT NULL,
                key TEXT NOT NULL,
                content_type TEXT,
                created INTEGER NOT NULL
             );
             CREATE TABLE multipart_parts (
                upload_id TEXT NOT NULL,
                part_number INTEGER NOT NULL,
                size INTEGER NOT NULL,
                part_etag TEXT NOT NULL,
                first_chunk INTEGER NOT NULL,
                chunk_count INTEGER NOT NULL,
                PRIMARY KEY (upload_id, part_number)
             );",
        )
        .unwrap();
    }

    #[test]
    fn test_new_database_has_versioned_schema() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("fresh.db");
        let db = MetadataDb::open(db_path.to_str().unwrap()).unwrap();

        let version: i64 = db
            .with_conn(|conn| {
                Ok(conn.query_row(
                    "SELECT version FROM schema_version LIMIT 1",
                    [],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert!(version >= 2, "schema_version should be >= 2, got {}", version);

        // files + versions tables exist
        for table in ["files", "versions", "buckets", "multipart_uploads", "multipart_parts"] {
            let exists = db
                .with_conn(|conn| MetadataDb::table_exists(conn, table))
                .unwrap();
            assert!(exists, "missing table {}", table);
        }
    }

    #[test]
    fn test_old_schema_migration() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("migrate.db");

        create_old_schema(db_path.to_str().unwrap());

        // Seed an object in the legacy `objects` table.
        {
            let conn = Connection::open(db_path.to_str().unwrap()).unwrap();
            conn.execute(
                "INSERT INTO objects (bucket_name, key, size, etag, last_modified, content_type, account_email, remote_path)
                 VALUES ('b', 'k.txt', 123, 'e', '2026-01-01', NULL, 'a@x', '/r/k.txt')",
                [],
            )
            .unwrap();
        }

        // Open triggers migration to v3 (current latest).
        let db = MetadataDb::open(db_path.to_str().unwrap()).unwrap();

        let version: i64 = db
            .with_conn(|conn| {
                Ok(conn.query_row(
                    "SELECT version FROM schema_version LIMIT 1",
                    [],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(version, 3, "migration should set version to 3");

        // The legacy object became version 1 of a file, with its blob preserved.
        let obj = db.get_object("b", "k.txt").unwrap().expect("object migrated");
        assert_eq!(obj.version, 1);
        assert_eq!(obj.size, 123);
        assert_eq!(obj.remote_path, "/r/k.txt");
        assert_eq!(obj.account_email, "a@x");

        // Legacy table was renamed, not dropped.
        let legacy_exists = db
            .with_conn(|conn| MetadataDb::table_exists(conn, "objects_legacy"))
            .unwrap();
        assert!(legacy_exists);
    }

    #[test]
    fn test_reserve_commit_get_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let db = MetadataDb::open(dir.path().join("test.db").to_str().unwrap()).unwrap();
        db.create_bucket("test").unwrap();

        let (version, path) = db
            .reserve_version("test", "hello.txt", "acct1", "/mnt/acct1")
            .unwrap();
        assert_eq!(version, 1);
        assert!(path.ends_with("hello.txt.v1.c1"), "path was {}", path);

        db.commit_version(
            "test",
            "hello.txt",
            version,
            12,
            "etag1",
            "2026-01-01",
            None,
            &path,
        )
        .unwrap();

        let obj = db.get_object("test", "hello.txt").unwrap().unwrap();
        assert_eq!(obj.version, 1);
        assert_eq!(obj.size, 12);
        assert_eq!(obj.remote_path, path);
        assert_eq!(obj.account_email, "acct1");
    }

    #[test]
    fn test_overwrite_increments_version_and_supersedes() {
        let dir = tempfile::tempdir().unwrap();
        let db = MetadataDb::open(dir.path().join("test.db").to_str().unwrap()).unwrap();
        db.create_bucket("test").unwrap();

        let (v1, p1) = db.reserve_version("test", "a.txt", "acct1", "/mnt/a").unwrap();
        db.commit_version("test", "a.txt", v1, 10, "e1", "2026-01-01", None, &p1)
            .unwrap();

        let (v2, p2) = db.reserve_version("test", "a.txt", "acct2", "/mnt/b").unwrap();
        assert_eq!(v2, 2);
        db.commit_version("test", "a.txt", v2, 7, "e2", "2026-01-02", None, &p2)
            .unwrap();

        // Current version resolves to v2.
        let obj = db.get_object("test", "a.txt").unwrap().unwrap();
        assert_eq!(obj.version, 2);
        assert_eq!(obj.size, 7);
        assert_eq!(obj.remote_path, p2);

        // v1 is superseded (not current), and appears in the orphan list.
        let now = chrono::Utc::now().timestamp_millis();
        let orphans = db.list_orphan_versions(now + 1).unwrap();
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].version, 1);
    }

    #[test]
    fn test_delete_marks_superseded() {
        let dir = tempfile::tempdir().unwrap();
        let db = MetadataDb::open(dir.path().join("test.db").to_str().unwrap()).unwrap();
        db.create_bucket("test").unwrap();

        let (v1, p1) = db.reserve_version("test", "a.txt", "acct1", "/mnt/a").unwrap();
        db.commit_version("test", "a.txt", v1, 10, "e1", "2026-01-01", None, &p1)
            .unwrap();

        db.delete_object("test", "a.txt").unwrap();
        assert!(db.get_object("test", "a.txt").unwrap().is_none());

        // The version is now superseded → vacuum reclaims it.
        let now = chrono::Utc::now().timestamp_millis();
        let orphans = db.list_orphan_versions(now + 1).unwrap();
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].version, 1);
    }

    #[test]
    fn test_build_remote_path() {
        let p = build_remote_path("/mnt/x", "video", "sub/foo.mkv", 3);
        assert_eq!(p, "/mnt/x/video/sub/foo.mkv.v3.c1");

        // Long key falls back to hash name.
        let long_key = "x".repeat(300);
        let p2 = build_remote_path("/mnt/x", "video", &long_key, 1);
        assert!(p2.len() < 255, "path too long: {} ({} bytes)", p2, p2.len());
        assert!(p2.ends_with(".v1.c1"));
        assert!(!p2.contains(&long_key), "hash fallback should not embed the long key");
    }

    #[test]
    fn test_count_and_size() {
        let dir = tempfile::tempdir().unwrap();
        let db = MetadataDb::open(dir.path().join("test.db").to_str().unwrap()).unwrap();
        db.create_bucket("test").unwrap();

        let (v1, p1) = db.reserve_version("test", "a.txt", "a1", "/mnt/a").unwrap();
        db.commit_version("test", "a.txt", v1, 100, "e1", "2026-01-01", None, &p1).unwrap();
        let (v2, p2) = db.reserve_version("test", "b.txt", "a1", "/mnt/a").unwrap();
        db.commit_version("test", "b.txt", v2, 200, "e2", "2026-01-01", None, &p2).unwrap();

        assert_eq!(db.count_objects("test").unwrap(), 2);
        assert_eq!(db.bucket_total_size("test").unwrap(), 300);
        assert_eq!(db.count_objects_for_account("a1").unwrap(), 2);
        assert_eq!(db.account_total_size("a1").unwrap(), 300);
    }
}
