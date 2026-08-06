use rusqlite::{params, Connection};


#[derive(Debug, Clone)]
pub struct BucketRecord {
    pub name: String,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct FileRecord {
    pub bucket_name: String,
    pub key: String,
    pub size: i64,
    pub etag: String,
    pub last_modified: String,
    pub content_type: Option<String>,
    pub created_at: String,
    pub owner: Option<String>,
    pub storage_type: String,
}

#[derive(Debug, Clone)]
pub struct ChunkRecord {
    pub bucket_name: String,
    pub key: String,
    pub chunk_index: i32,
    pub size: i64,
    pub checksum: String,
    pub is_parity: bool,
    pub account_email: String,
    pub remote_path: String,
}

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
}

/// Thread-safe metadata database wrapper.
/// All operations use spawn_blocking so they won't deadlock the async runtime.
#[derive(Clone)]
pub struct MetadataDb {
    path: String,
}

impl MetadataDb {
    pub fn open(path: &str) -> anyhow::Result<Self> {
        // Initialize the database (blocking init is fine)
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
            CREATE TABLE IF NOT EXISTS objects (
                bucket_name TEXT NOT NULL,
                key TEXT NOT NULL,
                size INTEGER NOT NULL DEFAULT 0,
                etag TEXT NOT NULL,
                last_modified TEXT NOT NULL,
                content_type TEXT,
                account_email TEXT NOT NULL,
                remote_path TEXT NOT NULL,
                PRIMARY KEY (bucket_name, key),
                FOREIGN KEY (bucket_name) REFERENCES buckets(name) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_objects_bucket ON objects(bucket_name);
            CREATE INDEX IF NOT EXISTS idx_objects_account ON objects(account_email);
            CREATE INDEX IF NOT EXISTS idx_objects_prefix ON objects(bucket_name, key);

            CREATE TABLE IF NOT EXISTS files (
                bucket_name TEXT NOT NULL,
                key TEXT NOT NULL,
                size INTEGER NOT NULL DEFAULT 0,
                etag TEXT NOT NULL,
                last_modified TEXT NOT NULL,
                content_type TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                owner TEXT,
                storage_type TEXT NOT NULL DEFAULT 'whole',
                PRIMARY KEY (bucket_name, key)
            );

            CREATE TABLE IF NOT EXISTS chunks (
                bucket_name TEXT NOT NULL,
                key TEXT NOT NULL,
                chunk_index INTEGER NOT NULL,
                size INTEGER NOT NULL DEFAULT 0,
                checksum TEXT NOT NULL,
                is_parity INTEGER NOT NULL DEFAULT 0,
                account_email TEXT NOT NULL,
                remote_path TEXT NOT NULL,
                PRIMARY KEY (bucket_name, key, chunk_index)
            );

            CREATE INDEX IF NOT EXISTS idx_chunks_account ON chunks(account_email);
            CREATE INDEX IF NOT EXISTS idx_chunks_file ON chunks(bucket_name, key);
            CREATE INDEX IF NOT EXISTS idx_files_bucket ON files(bucket_name);

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
                first_chunk INTEGER NOT NULL,
                chunk_count INTEGER NOT NULL,
                PRIMARY KEY (upload_id, part_number)
            );
            ",
        )?;
        Ok(Self { path: path.to_string() })
    }

    pub fn with_conn<F, T>(&self, f: F) -> anyhow::Result<T>
    where
        F: FnOnce(&Connection) -> anyhow::Result<T>,
    {
        let conn = Connection::open(&self.path)?;
        f(&conn)
    }

    // ---- Bucket operations ----

    pub fn bucket_exists(&self, name: &str) -> anyhow::Result<bool> {
        self.with_conn(|conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM buckets WHERE name = ?1", params![name], |row| row.get(0),
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
                Ok(Some(BucketRecord { name: row.get(0)?, created_at: row.get(1)? }))
            } else {
                Ok(None)
            }
        })
    }

    pub fn list_buckets(&self) -> anyhow::Result<Vec<BucketRecord>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT name, created_at FROM buckets ORDER BY name")?;
            let rows = stmt.query_map([], |row| {
                Ok(BucketRecord { name: row.get(0)?, created_at: row.get(1)? })
            })?;
            let mut buckets = Vec::new();
            for row in rows { buckets.push(row?); }
            Ok(buckets)
        })
    }

    pub fn list_objects(&self, bucket: &str, prefix: Option<&str>, max_keys: i64) -> anyhow::Result<Vec<ObjectRecord>> {
        self.with_conn(|conn| {
            // Query both objects (whole-file) and files (chunked) tables
            let (sql, params_vec): (&str, Vec<Box<dyn rusqlite::types::ToSql>>) = if let Some(p) = prefix {
                let pattern = format!("{}%", p);
                ("SELECT key, size, etag, last_modified, content_type, '' AS account_email, 'chunked://' || bucket_name || '/' || key AS remote_path, bucket_name
                  FROM files WHERE bucket_name = ?1 AND key LIKE ?2 AND storage_type = 'chunked'
                  UNION ALL
                  SELECT key, size, etag, last_modified, content_type, account_email, remote_path, bucket_name
                  FROM objects WHERE bucket_name = ?1 AND key LIKE ?2
                  ORDER BY key LIMIT ?3",
                 vec![Box::new(bucket.to_string()), Box::new(pattern), Box::new(max_keys)])
            } else {
                ("SELECT key, size, etag, last_modified, content_type, '' AS account_email, 'chunked://' || bucket_name || '/' || key AS remote_path, bucket_name
                  FROM files WHERE bucket_name = ?1 AND storage_type = 'chunked'
                  UNION ALL
                  SELECT key, size, etag, last_modified, content_type, account_email, remote_path, bucket_name
                  FROM objects WHERE bucket_name = ?1
                  ORDER BY key LIMIT ?2",
                 vec![Box::new(bucket.to_string()), Box::new(max_keys)])
            };
            let mut stmt = conn.prepare(sql)?;
            let param_refs: Vec<&dyn rusqlite::types::ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();
            let rows = stmt.query_map(param_refs.as_slice(), |row| {
                Ok(ObjectRecord {
                    key: row.get(0)?,
                    size: row.get(1)?,
                    etag: row.get(2)?,
                    last_modified: row.get(3)?,
                    content_type: row.get(4)?,
                    account_email: row.get::<_, String>(5).unwrap_or_default(),
                    remote_path: row.get::<_, String>(6).unwrap_or_default(),
                    bucket_name: row.get(7)?,
                })
            })?;
            let mut objects = Vec::new();
            for row in rows { objects.push(row?); }
            Ok(objects)
        })
    }

    pub fn put_object(&self, bucket: &str, key: &str, size: i64, etag: &str, last_modified: &str, account_email: &str, remote_path: &str, content_type: Option<&str>) -> anyhow::Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO objects (bucket_name, key, size, etag, last_modified, content_type, account_email, remote_path)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![bucket, key, size, etag, last_modified, content_type, account_email, remote_path],
            )?;
            Ok(())
        })
    }

    pub fn get_object(&self, bucket: &str, key: &str) -> anyhow::Result<Option<ObjectRecord>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT key, size, etag, last_modified, content_type, account_email, remote_path, bucket_name
                 FROM objects WHERE bucket_name = ?1 AND key = ?2",
            )?;
            let mut rows = stmt.query(params![bucket, key])?;
            if let Some(row) = rows.next()? {
                Ok(Some(ObjectRecord { key: row.get(0)?, size: row.get(1)?, etag: row.get(2)?, last_modified: row.get(3)?, content_type: row.get(4)?, account_email: row.get(5)?, remote_path: row.get(6)?, bucket_name: row.get(7)? }))
            } else { Ok(None) }
        })
    }

    pub fn delete_object(&self, bucket: &str, key: &str) -> anyhow::Result<()> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM objects WHERE bucket_name = ?1 AND key = ?2", params![bucket, key])?;
            conn.execute("DELETE FROM chunks WHERE bucket_name = ?1 AND key = ?2", params![bucket, key])?;
            conn.execute("DELETE FROM files WHERE bucket_name = ?1 AND key = ?2", params![bucket, key])?;
            Ok(())
        })
    }

    pub fn list_all_objects(&self) -> anyhow::Result<Vec<ObjectRecord>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT key, size, etag, last_modified, content_type, account_email, remote_path, bucket_name
                 FROM objects ORDER BY bucket_name, key"
            )?;
            let rows = stmt.query_map([], |row| {
                Ok(ObjectRecord {
                    key: row.get(0)?,
                    size: row.get(1)?,
                    etag: row.get(2)?,
                    last_modified: row.get(3)?,
                    content_type: row.get(4)?,
                    account_email: row.get::<_, String>(5).unwrap_or_default(),
                    remote_path: row.get::<_, String>(6).unwrap_or_default(),
                    bucket_name: row.get(7)?,
                })
            })?;
            let mut objects = Vec::new();
            for row in rows { objects.push(row?); }
            Ok(objects)
        })
    }

    pub fn delete_all_objects(&self, bucket: &str) -> anyhow::Result<()> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM objects WHERE bucket_name = ?1", params![bucket])?;
            conn.execute("DELETE FROM chunks WHERE bucket_name = ?1", params![bucket])?;
            conn.execute("DELETE FROM files WHERE bucket_name = ?1", params![bucket])?;
            Ok(())
        })
    }

    pub fn count_objects(&self, bucket: &str) -> anyhow::Result<i64> {
        self.with_conn(|conn| {
            conn.query_row("SELECT COUNT(*) FROM objects WHERE bucket_name = ?1", params![bucket], |row| row.get(0)).map_err(anyhow::Error::from)
        })
    }

    pub fn bucket_total_size(&self, bucket: &str) -> anyhow::Result<i64> {
        self.with_conn(|conn| {
            conn.query_row("SELECT COALESCE(SUM(size), 0) FROM objects WHERE bucket_name = ?1", params![bucket], |row| row.get(0)).map_err(anyhow::Error::from)
        })
    }

    pub fn count_objects_for_account(&self, email: &str) -> anyhow::Result<i64> {
        self.with_conn(|conn| {
            conn.query_row("SELECT COUNT(*) FROM objects WHERE account_email = ?1", params![email], |row| row.get(0)).map_err(anyhow::Error::from)
        })
    }

    pub fn account_total_size(&self, email: &str) -> anyhow::Result<i64> {
        self.with_conn(|conn| {
            conn.query_row("SELECT COALESCE(SUM(size), 0) FROM objects WHERE account_email = ?1", params![email], |row| row.get(0)).map_err(anyhow::Error::from)
        })
    }

    // ---- Multipart upload (streaming) helpers ----

    /// Register a new in-progress multipart upload (on-disk so it survives restarts).
    pub fn create_multipart(&self, upload_id: &str, bucket: &str, key: &str, content_type: Option<&str>) -> anyhow::Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO multipart_uploads (upload_id, bucket, key, content_type, created) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![upload_id, bucket, key, content_type, chrono::Utc::now().timestamp()],
            )?;
            Ok(())
        })
    }

    /// Get an in-progress multipart upload. Returns (bucket, key, content_type).
    pub fn get_multipart(&self, upload_id: &str) -> anyhow::Result<Option<(String, String, Option<String>)>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT bucket, key, content_type FROM multipart_uploads WHERE upload_id = ?1")?;
            let mut rows = stmt.query(params![upload_id])?;
            if let Some(row) = rows.next()? {
                Ok(Some((row.get(0)?, row.get(1)?, row.get(2)?)))
            } else {
                Ok(None)
            }
        })
    }

    /// Remove an in-progress multipart upload and all its part records.
    pub fn delete_multipart(&self, upload_id: &str) -> anyhow::Result<()> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM multipart_uploads WHERE upload_id = ?1", params![upload_id])?;
            conn.execute("DELETE FROM multipart_parts WHERE upload_id = ?1", params![upload_id])?;
            Ok(())
        })
    }

    /// Store the metadata for a single stored part. The part bytes already live
    /// on backends as chunks; this records how to stitch them on Complete.
    pub fn store_multipart_part(
        &self,
        upload_id: &str,
        part_number: u64,
        size: i64,
        part_etag: &str,
        first_chunk: i32,
        chunk_count: i32,
    ) -> anyhow::Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO multipart_parts (upload_id, part_number, size, part_etag, first_chunk, chunk_count)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![upload_id, part_number as i64, size, part_etag, first_chunk, chunk_count],
            )?;
            Ok(())
        })
    }

    /// Fetch ordered part records: (part_number, size, part_etag, first_chunk, chunk_count).
    pub fn list_multipart_parts(&self, upload_id: &str) -> anyhow::Result<Vec<(u64, i64, String, i32, i32)>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT part_number, size, part_etag, first_chunk, chunk_count FROM multipart_parts
                 WHERE upload_id = ?1 ORDER BY part_number",
            )?;
            let rows = stmt.query_map(params![upload_id], |row| {
                Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)?, row.get::<_, String>(2)?, row.get::<_, i32>(3)?, row.get::<_, i32>(4)?))
            })?;
            let mut v = Vec::new();
            for r in rows { v.push(r?); }
            Ok(v)
        })
    }

    /// Fetch chunk metadata rows for an object key, ordered by chunk_index.
    pub fn list_chunks_for_key(&self, bucket: &str, key: &str) -> anyhow::Result<Vec<(i32, i64, String, String, String)>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT chunk_index, size, checksum, account_email, remote_path FROM chunks
                 WHERE bucket_name = ?1 AND key = ?2 ORDER BY chunk_index",
            )?;
            let rows = stmt.query_map(params![bucket, key], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
            })?;
            let mut v = Vec::new();
            for r in rows { v.push(r?); }
            Ok(v)
        })
    }
}
