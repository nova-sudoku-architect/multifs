# docs/notes — Historical Notes

Point-in-time design notes, proposals, and audit records. **Not** the source of
truth for how the system works today — that's [`../architecture.md`](../architecture.md)
and the top-level [`README.md`](../../README.md). These files are kept for
provenance; some describe designs that shipped, some that were superseded.

| Note | What it is | Status |
|------|------------|--------|
| [`2026-08-22-william-requests.md`](2026-08-22-william-requests.md) | Dated checklist of requested work (S3 ops, features) | Historical |
| [`PROPOSAL-2026-08-21.md`](PROPOSAL-2026-08-21.md) | Five feature enhancements (folder size, symlinks, prev/next, …) | Mostly shipped |
| [`S3-ROADMAP.md`](S3-ROADMAP.md) | S3 operations roadmap + live-verification status | Shipped |
| [`audit-findings-2026-08-22.md`](audit-findings-2026-08-22.md) | Read-only audit of `multifs audit` / orphan detection | Reference |
| [`web-ui-pagination.md`](web-ui-pagination.md) | Pagination + virtual-scroll design | **Superseded / on hold** |

When a note's content becomes authoritative, fold it into `docs/architecture.md`
and leave the note here as history.
