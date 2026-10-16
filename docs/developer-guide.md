# MultiFS — Developer Guide

For changing the code: how it is laid out, how requests flow, and how to build/test it.
Deep internals (data model, MVCC paths, GC, backend trait) live in
[architecture.md](architecture.md). The component diagram is in the
[README](../README.md#architecture-at-a-glance).

## Data-flow diagrams

### 1. Upload (`PutObject`)

A user uploads an object; the request streams through the S3 handler into the
engine, which reserves a version, streams the body to a chosen backend, then
commits.

```mermaid
sequenceDiagram
    autonumber
    participant C as S3 client
    participant H as S3 handler<br/>(server/s3/mod.rs)
    participant E as StorageEngine
    participant P as Placement
    participant M as MetadataDb (SQLite)
    participant B as Backend (pCloud / local)

    C->>H: PUT /bucket/key (body + Content-Length)
    H->>H: resolve content-type + charset
    Note over H: text → buffer (detect charset)<br/>binary + Content-Length → stream
    H->>E: put_object_stream(bucket, key, …)
    E->>M: ensure_bucket()
    E->>P: pick_backend()
    P-->>E: backend index (see diagram 6)
    E->>M: reserve_version() → (version, remote_path)  [status=pending]
    E->>B: upload_stream(remote_path, body, len)
    B-->>E: (actual_path, etag=SHA-256, size)
    E->>M: commit_version()  [status=committed, files ptr flipped,<br/>prev version superseded_at=now]
    E->>M: set_checksum() + set_charset()
    E->>M: touch_folder() (stamp ancestors' last_modified)
    E-->>H: ObjectInfo
    H-->>C: 200 OK + ETag
    Note over E,B: On error the version stays `pending`<br/>and is reclaimed later by vacuum.
```

### 2. Download (`GetObject`, with Range)

The read path resolves the live version once, so an in-flight download keeps
streaming even if a concurrent write flips the pointer.

```mermaid
sequenceDiagram
    autonumber
    participant C as S3 client / browser
    participant H as S3 / Web handler
    participant E as StorageEngine
    participant M as MetadataDb
    participant B as Backend

    C->>H: GET /bucket/key  (optional Range: bytes=start-end)
    H->>E: head_object() (size, content-type, charset)
    E->>M: files.current_version → versions.remote_path
    M-->>E: account_email + remote_path
    alt single blob
        E->>B: download_stream(remote_path, range_start, range_end)
        B-->>E: byte stream (Range forwarded to pCloud CDN)
    else multipart composite (remote_path = __mp__/multipart-<id>)
        E->>M: list_multipart_parts(upload_id)
        loop each part in order
            E->>B: download part + slice across boundaries
        end
    end
    E-->>H: streamed bytes
    H-->>C: 200 (or 206 Partial Content) + Content-Range
```

### 3. Multipart upload

rclone (and any S3 client copying files > 64 MB) uses multipart. Each part is a
standalone blob and may land on a *different* backend; `Complete` keeps the
part rows so later reads can assemble the object.

```mermaid
sequenceDiagram
    autonumber
    participant C as S3 client
    participant H as S3 handler
    participant E as StorageEngine
    participant M as MetadataDb
    participant B as Backends

    C->>H: POST /b/key?uploads
    H->>M: create_multipart() → UploadId
    H-->>C: <UploadId>

    loop each part
        C->>H: PUT /b/key?partNumber=N&uploadId=X
        H->>E: upload_part()
        E->>M: reserve_version(staging)
        E->>B: upload_stream(__mp__/<id>/<N>)  [per-part backend pick]
        E->>M: store_multipart_part(account, path, md5)
        H-->>C: ETag (part MD5)
    end

    C->>H: POST /b/key?uploadId=X (part list)
    H->>E: complete_multipart_upload()
    E->>M: compute multipart ETag, commit version<br/>(remote_path = staging dir, parts retained)
    H-->>C: 200 + final ETag

    Note over C,M: DELETE ?uploadId=X (abort) deletes part blobs + rows.<br/>Abandoned uploads (>24h) are swept by vacuum.
```

### 4. Web UI folder listing

The read-only UI (GET-only) lists a folder's direct children. To keep a wide
folder fast, child counts and per-folder metadata are fetched in **two batched
queries** rather than one round-trip per child.

```mermaid
sequenceDiagram
    autonumber
    participant BR as Browser (index.html)
    participant R as web router<br/>(server/web/mod.rs)
    participant E as StorageEngine
    participant M as MetadataDb

    BR->>R: GET /api/list?bucket=video&prefix=blor-116/
    R->>E: list_objects_symlink_aware(bucket, prefix, MAX_KEYS=100000)
    E->>M: list_objects() (rows + mirror metadata)
    M-->>E: objects (+ truncated flag)
    R->>R: group_objects_by_prefix() → (folders, files)
    R->>E: symlink_prefixes_under()  (merge tag folders)
    R->>E: folder_meta_batch(prefixes)      [1 query]
    R->>E: count_children_batch(prefixes)   [1 query]
    E->>M: batched lookups
    M-->>R: counts + cover/summary/gif + last_modified
    R-->>BR: JSON { buckets/prefixes, files, folder, truncated }
    Note over BR,R: Download uses GET /api/download?bucket=&key=<br/>which honors Range and streams via get_object_stream().
```

### 5. Version lifecycle (MVCC + GC)

```mermaid
stateDiagram-v2
    [*] --> pending: reserve_version<br/>(upload starts)
    pending --> committed: upload ok → commit_version<br/>(files.current_version = v)
    pending --> [*]: vacuum after 1h<br/>(abandoned upload)
    committed --> superseded: a newer version commits<br/>(superseded_at = now)
    committed --> deleted: object DELETE<br/>(files row removed)
    superseded --> [*]: vacuum after 10min grace<br/>(blob + row reclaimed)
    deleted --> [*]: vacuum after grace<br/>(blob reclaimed)
```

### 6. Backend placement decision

```mermaid
flowchart TD
    START([put_object / upload_part]) --> STRAT{placement_strategy}
    STRAT -->|round-robin| RR["idx = counter++ % backends.len()"]
    STRAT -->|utilization · default| Q["refresh_quotas() (cached)"]
    Q --> LVL["iterate priority tiers ascending<br/>(0 = preferred first)"]
    LVL --> BEST["pick least-full backend in tier<br/>(fill_ratio = used / quota)"]
    BEST --> FULL{fill_ratio < 1.0?}
    FULL -->|yes| PICK[use this backend]
    FULL -->|no| NEXTTIER{more tiers?}
    NEXTTIER -->|yes| LVL
    NEXTTIER -->|no| LEAST["all full → least-full overall (best effort)"]
    LEAST --> PICK
    RR --> PICK
    PICK --> END([reserve_version on chosen backend])
```

Net effect of the default (`utilization`) strategy: pCloud absorbs every write
while it has free space; the local disk (priority `1`) is a last-resort
overflow tier.


## Repository layout

```
src/
├── main.rs                 # clap CLI dispatch
├── config.rs               # Config model (server/storage/tls/accounts)
├── cli/                    # one module per subcommand (serve, account, bucket, …)
├── server/
│   ├── mod.rs              # shared helpers (range parse, prefix grouping, charset)
│   ├── s3/mod.rs           # S3 API router + handlers
│   └── web/                # read-only web UI router + embedded index.html
└── storage/
    ├── engine.rs           # StorageEngine — the single choke point
    ├── metadata.rs         # MetadataDb (SQLite, migrations, queries)
    ├── backends/           # StorageBackend trait + pcloud / local_disk impls
    └── pcloud/             # pCloud HTTP client + OAuth
tests/                      # integration tests (S3, WebDAV, pCloud, shell)
docs/
├── architecture.md         # deep reference
└── notes/                  # historical proposals / audits / designs
```


## Running tests

```bash
cargo test --lib            # unit tests (141 passing) — count must match docs/architecture.md
cargo test                  # unit + integration tests
cargo test -- --nocapture   # with output
cargo clippy                # lints
```


## Adding a storage backend

Implement the `StorageBackend` trait (`upload`, `upload_stream`, `download`,
`download_stream`, `list`, `stat`, `delete`, `server_side_copy`, …) in
`src/storage/backends/`, set its `backend_type` string, and wire it in
`StorageEngine::build_backends`. See
[architecture.md → Backend Interface](architecture.md#backend-interface-storagebackend-trait)
and [→ How a backend works](architecture.md#how-a-backend-works-protocol).
