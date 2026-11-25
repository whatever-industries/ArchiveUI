# Archive UI

A small desktop app (Tauri + Rust) for macOS, Windows and Linux that uploads
items to [archive.org](https://archive.org) from one window — sign in, fill in
the item metadata, pick folders or files, and upload.

It talks to archive.org directly (its sign-in service and S3-compatible upload
API), so there is nothing else to install — no Python, no `ia` CLI.

## Interface

- **Account** — archive.org email + password. Signing in fetches the account's
  upload keys, which are kept in memory for the session only.
- **Source** — select (or drag in) folders and/or files. Folders upload
  recursively with their structure preserved (hidden files skipped). The
  resolved file list and total size are shown before upload.
- **Required metadata** — Identifier, Title, Description, Topics (`subject`,
  comma-separated), and Media Type (combo box of archive.org media types).
- **Optional metadata** — Creator, Collection (must be a
  valid archive.org collection identifier, e.g. `open_source_software`), Date
  (`YYYY`, `YYYY-MM-DD`, or `YYYY-MM-DD HH:MM:SS`), License (combo box of
  Creative Commons options → stored as `licenseurl`), and Language.

The **From Title** button derives a sanitized identifier from the title, and
**Check** asks archive.org whether the identifier is still available (and, if
it's taken, whether the signed-in account owns it).

## Queue

Items upload **one at a time**. Fill in an item's metadata and source, click
**Add to Queue**, then repeat for the next item — the per-item fields clear
while the account and "sticky" metadata (media type, collection, license,
language, creator) carry over. **Start Upload** signs in once, then processes
the queue top to bottom, showing each item's live status (Queued → Uploading →
Done/Failed). **Stop after current** halts once the in-flight item finishes;
**Clear finished** / **Clear all** prune the list.

Files go up one at a time. Files already in the item (same name, size and MD5)
are skipped, busy-server responses are retried automatically, and derive is
queued once, after the last file.

## Updates

The app checks for a new release on launch and shows an **Update** button above
the log when one is available; it downloads, installs and relaunches itself.

## Develop / run

```sh
cargo tauri dev      # or: cd src-tauri && cargo run
```

## Build

Releases are built by GitHub Actions on every push to `main`. A local
`cargo tauri build` needs the updater signing key in the environment:

```sh
TAURI_SIGNING_PRIVATE_KEY="$(cat ~/.tauri/archive-ui.key)" \
TAURI_SIGNING_PRIVATE_KEY_PASSWORD="$(cat ~/.tauri/archive-ui.key.password)" \
cargo tauri build
```

## License

Licensed under the [MIT License](LICENSE).
