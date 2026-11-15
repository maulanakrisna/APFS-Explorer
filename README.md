# APFS-Explorer

A Windows Explorer–style, **read-only** browser for Apple APFS volumes, written in Rust with a native Win32 UI (no GUI framework).

> **Status: experimental.** The APFS parser was written from the public Apple File System Reference and has only had limited testing. Treat it as a starting point, not a finished tool.
> **Not affiliated with or endorsed by Apple.** "APFS" and "Apple" are trademarks of Apple Inc.

## Features

- Finds APFS partitions on physical disks (`\\.\PhysicalDriveN`, via GPT) and in raw disk images
- Lists volumes, browses folders (double-click), shows size / modified time (UTC) / kind
- Copies files and folders out of the volume to a Windows folder
- Own minimal APFS reader: container superblock + checkpoint scan, object map, volume superblock, filesystem B-tree, directory records, file extents. No third-party parser dependency.

## Limitations

- **Read-only.** There is no write support (see roadmap below).
- Encrypted volumes (FileVault) are not supported.
- APFS-compressed files (decmpfs; common among macOS system files) cannot be extracted.
- No snapshots, Fusion drives, or Fletcher-64 checksum verification.
- Only raw images; compressed `.dmg` (UDIF) is not supported.
- No folder tree pane yet; disk scanning runs on the UI thread.

## Build

Requires Windows and Rust (MSVC toolchain).

```
cargo run --release
```

The app manifest requests Administrator rights because opening `\\.\PhysicalDriveN` requires them. If Windows Smart App Control or another Application Control policy blocks unsigned build output (error 4551), builds will fail until the policy allows it.

## Safety

This tool only opens disks for **reading**. Still, do not point experimental software at irreplaceable data without a backup.

## Roadmap: write support (help wanted)

Writing APFS is hard and risky: it is copy-on-write, so even adding one file requires a full transaction. A careless implementation can make a volume unmountable on macOS or lose data. If you take this on, a careful staged approach is suggested:

1. **Foundations (no mutation):** Fletcher-64 verification on read; parse the space manager (bitmaps, chunk info, free queue); a write-capable device layer that is **only enabled for image files**, never physical disks.
2. **Simple operations on image copies:** create folder, rename, delete.
3. **Copy files into a volume.**
4. **Validate** every result with Apple's tooling (`fsck_apfs` on macOS) before trusting it. Only then consider physical-disk writes, behind strong warnings and explicit confirmation.

A full transaction involves: allocating free blocks, writing new filesystem B-tree nodes up to the root, updating the object map, writing a new volume superblock (with updated counters), writing a checkpoint map and a new container superblock into the checkpoint ring, and checksumming every block written. Restrict writes to unencrypted Data volumes without snapshots at first.

## Contributing

Issues and pull requests are welcome. Useful first contributions:

- Test images generated on a Mac (small APFS volumes with known contents) and automated tests against them
- Checksum verification, decmpfs support, FileVault-aware error handling
- Folder tree pane and background disk scanning
- The write roadmap above

## License

MIT
