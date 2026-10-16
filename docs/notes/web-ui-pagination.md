# Web UI — Pagination & Virtual Scroll (design)

Status: **superseded / on hold** — William chose the batched-listing route
(option b) over pagination on 2026-10-15. The N+1 folder-listing cost that
motivated this design was addressed by `count_children_batch` +
`folder_meta_batch` (see `docs/architecture.md` → *Read-Only Web UI*), which cut
a wide-folder listing from ~5 s to tens of ms. The WIP code for this design is
preserved on branch `wip/web-pagination-20261014`. Kept for reference; revisit
only if unbounded payloads / DOM size become a problem at scale.
Owner: coder
Related: `src/server/web/mod.rs`, `src/server/web/index.html`, `src/storage/engine.rs`,
`src/storage/metadata.rs`, `src/server/mod.rs`.

## Motivation

The read-only web console (`/`, port 9001) renders a folder by fetching **every**
direct child in one JSON response and building the **full DOM** for all of them.

Today `/api/list` calls `list_objects_symlink_aware(bucket, prefix, None, 100_000)`:
it pulls up to **100 000 keys recursively** beneath the prefix, collapses them to
direct children with `group_objects_by_prefix`, then attaches per-folder metadata
(`folder_meta_map`) and a **separate `COUNT(*)` per folder**
(`count_direct_children`).

That is fine for small folders but degrades badly:

- **Payload** scales with total object count, not what's visible.
- **DOM** scales with the number of rows — thousands of nodes, slow first paint,
  heavy memory on mobile.
- `prefixes.contains()` in `group_objects_by_prefix` is **O(n²)**.
- One `COUNT` query **per folder** — hundreds of queries for a folder-heavy page.
- `loading="lazy"` only defers **images**; the JSON and the row nodes are still
  unbounded.

Goal: fetch only what is visible, and fetch more as the user scrolls — so an
enormous folder stays responsive and the source scan is bounded per request.

## API contract (new `/api/list`)

```
GET /api/list?bucket=<b>&prefix=<p>&sort=name|size|mtime|type&dir=asc|desc
              &limit=<n>&cursor=<opaque>
```

- `limit` — max **direct children** returned per stream, default **200**, max 1000.
- `cursor` — opaque continuation token from the previous response (see below).
- `sort`/`dir` — ordering key + direction. Default `name` / `asc`.

Response:

```jsonc
{
  "bucket": "...",
  "prefix": "...",
  "folder": { /* current folder's own cover/summary/gif/last_modified */ },
  "folders": [ { "prefix": "...", "cover_key": ..., "summary_key": ...,
                 "preview_gif_key": ..., "last_modified": 0,
                 "child_count": 3, "is_symlink": false } ],
  "files":   [ { "key": "...", "size": 0, "last_modified": "...",
                 "content_type": "..." } ],
  "next_cursor": "…opaque…",   // null when the stream is exhausted
  "sort_capped": false         // true when a non-name sort hit the cap (see below)
}
```

**One merged cursor** covers both `folders` and `files`, which are returned in
**key order** for `sort=name`. The client groups them for display (folders
section first, then files) but the transport is a single ordered stream — see
"Why one cursor".

### Cursor

`cursor` is the **last scanned source key** (base64 / opaque). The server resumes
strictly after it. It is not a raw offset (offsets re-scan and shift under writes).

`null`/absent ⇒ start at the beginning of the prefix.

## Algorithms

### `sort = name` (default) — streaming keyset, unbounded

Keys are stored flat in `files(bucket_name, key)` with PK `(bucket_name, key)`, so
a range seek `key ∈ (cursor, prefix_succ)` is index-friendly. `prefix_succ` is the
smallest key greater than every key under `prefix` (prefix with its last byte
incremented; `prefix` always ends in `/`, so `…/ ` → `…0`).

Walk direct children one at a time:

```
cursor := (request cursor, or prefix_succ of prefix)
emitted := 0
loop until emitted == limit or exhausted:
    row := SELECT key, … FROM files f JOIN versions v … 
           WHERE f.bucket_name = ?
             AND f.key > :cursor AND f.key < :prefix_succ
           ORDER BY f.key LIMIT 1
    if none: exhausted; break
    rest := row.key[prefix.len()..]
    if rest contains '/':
        seg := rest up to first '/'
        emit folder(prefix + seg + '/')
        cursor := successor(prefix + seg + '/')   // SKIP the whole subtree
    else:
        emit file(row)
        cursor := row.key
    emitted += 1
next_cursor := cursor if more rows remain, else null
```

**Why the subtree skip matters:** when a row belongs to a subfolder, we jump the
cursor to just past that folder's entire subtree (successor of its prefix) instead
of walking its children. So a subfolder with 100 000 objects costs **one**
iteration, not 100 000. Total scan cost for a page ≈ `O(direct children on the
page)` index probes — independent of how deep/large the subtrees are.

Result order is lexicographic by key (i.e. by name), exactly the current default.

### `sort = size | mtime | type` — capped enumerate + in-memory sort

Ordering by a non-key column can't use the skip trick (the skip assumes key
order). For these we enumerate **all** direct children (same loop, no early exit)
up to a **cap of 20 000**, sort in memory, then return the requested slice
(`cursor` here encodes the sort value + key of the last row, `(col, key)` keyset).

If the folder exceeds the cap, `sort_capped = true` and the UI shows a note
("showing first 20 000 after sort"). Rationale: enumeration is cheap thanks to the
skip trick, but unbounded memory/sort per request is not worth it for an
unbounded folder. Name sort remains fully unbounded.

> Decision D2 (see below) — cap value is configurable.

### Symlink (tag) folders

`/api/list` currently merges symlinked folders found under the prefix
(`symlink_prefixes_under`). Symlinks are few in practice, so the server fetches
them for the prefix and folds them into the **folders** stream (re-presented under
the link path, as today). They count against `limit`. A dedicated symlink index
(`idx_symlinks_bucket`) already exists.

### Folder metadata (batched)

For the folders **on the current page only**, fetch `folder_meta` and
`child_count` in batches:

- `folder_meta` — one `WHERE prefix IN (…)` query.
- `child_count` — one grouped query over the page's prefixes (replacing the
  current one-COUNT-per-folder).

## Storage layer changes

New `MetadataDb` methods (all using the `(bucket_name, key)` PK):

- `list_direct_children_page(bucket, prefix, after_key, limit) -> (Vec<Child>, bool)`
  — the merged folder/file keyset loop above (folders deduped via skip).
- `count_children_batch(bucket, prefixes) -> Map<prefix, i64>`.
- `folder_meta_for_prefixes(bucket, prefixes)` (may reuse `list_folder_meta`).

Engine wrappers mirror the existing symlink-aware behaviour
(`list_direct_children_symlink_aware`, `folder_meta_map`, etc.).

No new indexes are required for `sort=name`; the PK range scan suffices. Composite
indexes for non-name sorts are **not** needed because those pages are enumerated
then sorted in memory.

## Client changes (`index.html`)

- Keep the three modes (List / Preview / Names). All three consume the same paged
  API; each renders only the children it has loaded so far.
- **Virtual scroll**: a scroll container with a spacer sized to the loaded row
  count; render only the visible slice (+ small overscan). Rows are fixed height
  so offsets are computable. (Fallback: if a mode's rows vary in height, use a
  simple "append + window of N" renderer and load on scroll.)
- **Infinite scroll**: when the viewport nears the last loaded row and
  `next_cursor != null`, fetch the next page and append. Debounced; a loading
  shimmer sits at the end.
- **Sorting** (added earlier) now drives the **server** query: changing the sort
  column resets `cursor` and re-fetches from the top; the ▲/▼ UI is unchanged.
  Sort choice persists in `localStorage` (already does).
- Folder badge counts + cover thumbnails come from the page's folder metadata.
- Empty / single-page / capped states: plain messages.

## Why one cursor (not two)

The UI shows folders first and files second, which suggests two cursors. But
folders and files interleave in **key order**, so a single ordered stream is
simpler and avoids a second scan. The client waits for the folders section to
finish (first page usually contains them) before deciding layout; a fully
streamed alternative (append folders as found, files in a lazy list) is possible
if the interleave is awkward in practice.

## Compatibility

The old (unbounded) behaviour is **replaced** — no flag. Stale clients that omit
`limit`/`cursor` get a bounded first page (`limit` default) plus `next_cursor`, so
they still render the first page. The S3 listing path (its own continuation-token
pagination) is untouched.

## Decisions (defaults chosen; easy to change)

- **D1 — page size:** default **200** (max 1000).
- **D2 — non-name sort cap:** **20 000** direct children, then `sort_capped`.
- **D3 — search / jump-to-letter box:** **out of scope** for this change; noted as
  a follow-up (a `prefix`-seek box would be cheap to add later).
- **D4 — old behaviour:** **removed**, not kept behind a flag.

## Tests

`cargo test --lib` additions (update the count in `docs/architecture.md`):

- Pagination invariant: walking all pages yields exactly the direct children, no
  dups/misses — for a fixture with nested subtrees (verifies the skip trick).
- `sort=name` across page boundaries (keys straddling a subfolder prefix).
- Subtree skip: a subfolder with many objects costs one row.
- `sort=size|mtime|type` ordering + cap flag.
- Symlink folders appear once, under the link path.
- Empty folder, single-page, exact-boundary (`limit` == child count).
- `prefix_succ` boundary: siblings named `<prefix>-whatever` must NOT be included.

Manual: generate a synthetic folder (e.g. 200k keys), confirm payload size is
flat per page and first paint is fast; confirm scroll loads more.

## Docs to update in the same change

- `README.md` — features table (web UI row: paginated/virtualized).
- `docs/architecture.md` — Read-Only Web UI section: new `/api/list` contract,
  pagination algorithm, test count.

## Rollout

1. Land server-side pagination + tests (this doc's first half).
2. Land client virtual/infinite scroll.
3. Rebuild + deploy (UI is embedded via `include_str!`, so a rebuild is required),
   restart `multifs.service`.
4. Verify on the live endpoint with a large folder.
