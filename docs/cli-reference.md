# MultiFS — CLI Reference

Every `multifs` subcommand. Run `multifs <command> --help` for exact flags.

## Commands

```
multifs serve [--config <path>]   Start the daemon
multifs init [--config <path>] [--force]
                                  Initialize config + database
multifs check [--config <path>]   Validate config, test all accounts
multifs status                    Daemon health + account stats
multifs config show               Show current configuration
multifs config set <key> <value>  Set a config value (dot notation) — ⚠️ stub, not
                                  yet implemented (edit the config file directly)

multifs account list              List configured accounts
multifs account add <email> [--token <t>]
                                  Add an account (validates token, auto quota)
multifs account remove <email>    Remove an account from rotation
multifs account check <email>     Test token + show quota
multifs account refresh <email>   Refresh OAuth token

multifs bucket list               List buckets
multifs bucket create <name>      Create a bucket
multifs bucket delete <name> [--force]
                                  Delete a bucket (--force skips empty check)
multifs bucket info <name>        Bucket stats

multifs object list <bucket> [--prefix <p>] [--limit <n>]
                                  List objects
multifs object cp <src> <dst> [-r]
                                  Copy object (local ↔ remote)
multifs object rm <bucket>/<key>  Delete an object
multifs object info <bucket>/<key>
                                  Show object metadata
multifs object coalesce <bucket>/<key> | --all [--dry-run]
                                  Rewrite multipart object(s) into a single blob

multifs backend status             Fill levels for all backends (objects + parts)
multifs backend rebalance [--dry-run]
                                  Move data over-full → under-full backends
                                  (`shard` kept as a hidden-compatible alias)

multifs folder set-cover <bucket> <prefix> <key>
                                  Set a folder's cover image
multifs folder set-summary <bucket> <prefix> <key>
                                  Set a folder's summary document
multifs folder set-gif <bucket> <prefix> <key>
                                  Set a folder's preview GIF
multifs folder clear <bucket> <prefix>
                                  Clear all folder metadata
multifs folder backfill [--bucket <b>] [--dry-run]
                                  Scan objects and record cover/summary/GIF per folder
multifs folder list [--bucket <b>]  List recorded folder metadata

multifs link <bucket>/<key> <bucket>/<target>   Create a symlink (tag folder)
multifs link unlink <bucket>/<key>              Remove a symlink (target untouched)
multifs link list-links [--bucket <b>]          List symlinks

multifs audit scan <email> [--summary]          Find pCloud files NOT managed by MultiFS
multifs audit list-files <email>                List all files (managed + orphaned)
multifs audit reconcile [--account <email>]     Diff each account's files vs the DB
multifs audit cleanup [--account <email>] [--apply] [--verbose]
                                  Delete orphaned pCloud files (dry-run by default)

multifs import <email> <path> --bucket <b> [--key <k>]
                                  Register an existing pCloud file (metadata only)
multifs import <email> --scan [--bucket <b>] [--prefix <p>] [--dry-run]
                                  Bulk-import every unmanaged file in the account

multifs vacuum [--dry-run]        GC superseded + abandoned version blobs + abandoned multipart uploads

multifs checksum rebuild [path]   Recompute + store SHA-256 for one object
multifs checksum rebuild --all    Recompute + store SHA-256 for every object
multifs checksum verify [path]    Verify a stored checksum against live content
multifs checksum verify --all     Verify every object's checksum

multifs fsck [--checksums] [--fix] [--prune-missing] [--apply]
                                  Verify DB integrity + backend presence/size
                                  (--checksums also verify content; --fix repairs orphan
                                   multipart state + runs vacuum; --prune-missing [--apply]
                                   drops DB rows for blobs confirmed missing)
```

