# Archive.org Uploader

A small macOS desktop app (Tauri + Rust) for uploading items to
[archive.org](https://archive.org) from one window — sign in, fill in the item
metadata, pick a folder or individual files, and upload.

## Interface

- **Account** — archive.org username/email + password (used to configure the
  `ia` CLI for the session).
- **Source** — select a whole folder (uploaded recursively, hidden files
  skipped) *or* individual files. The resolved file list and total size are
  shown before upload.
- **Required metadata** — Identifier, Title, Description, Topics (`subject`,
  comma-separated), and Media Type (combo box of archive.org media types).
- **Optional metadata** (collapsed by default) — Creator, Collection (must be a
  valid archive.org collection identifier, e.g. `open_source_software`), Date
  (`YYYY`, `YYYY-MM-DD`, or `YYYY-MM-DD HH:MM:SS`), License (combo box of
  Creative Commons options → stored as `licenseurl`), and Language.

The **From Title** button derives a sanitized identifier from the title, and
**Check** pings archive.org (`ia metadata`) to report whether the identifier is
still available.

## Queue

Items upload **one at a time**. Fill in an item's metadata and source, click
**Add to Queue**, then repeat for the next item — the per-item fields clear
while the account and "sticky" metadata (media type, collection, license,
language, creator) carry over. **Start Upload** signs in once, then processes
the queue top to bottom, showing each item's live status (Queued → Uploading →
Done/Failed). **Stop after current** halts once the in-flight item finishes;
**Clear finished** / **Clear all** prune the list.

## Requirements

The [`ia` command-line tool](https://archive.org/developers/internetarchive/)
must be installed and on `PATH`:

```sh
pip install internetarchive
```

## Develop / run

```sh
cargo tauri dev      # or: cd src-tauri && cargo run
```

## Build

```sh
cargo tauri build
```

## License

Licensed under the [GNU General Public License v2.0](LICENSE).
