# macOS development preview

This backend synchronizes ordinary local folders. Files are downloaded in full;
it does not implement File Provider, placeholders, eviction, Finder badges, or
Finder context menus. Linux is outside this implementation's scope.

## Build on macOS

Install Xcode Command Line Tools, current stable Rust (the locked dependencies require at least 1.88), Node.js 22+, and Yarn 1.22.

```sh
yarn --cwd ui install --frozen-lockfile
npm --prefix ui run tauri -- dev
npm --prefix ui run tauri -- build --debug --bundles app
# Release installer (configure Developer ID signing/notarization for distribution):
npm --prefix ui run tauri -- build
```

The development bundle is `target/debug/bundle/macos/Cloudreve.app`.
The macOS bundle config currently targets macOS 12+; the actual supported OS/CPU
matrix still needs hardware validation. Build on each architecture or install
both Rust targets and use Tauri's `universal-apple-darwin` target.

Do not distribute the development bundle as a signed release. Developer ID
signing, notarization, Intel execution, and testing additional Cloudreve storage
providers remain release gates. The local-storage server smoke test is documented
in the File Provider development notes.

## Behavior and limits

- Use an existing dedicated local folder with enough space for the entire selected
  cloud folder. Sync roots cannot overlap or contain `~/.cloudreve` application state.
- Local watcher events and remote SSE trigger complete reconciliation. A 30-second
  rescan also repairs missed events. This first implementation favors correctness;
  it hashes local file content and serializes transfers with reconciliation.
  Large-tree performance optimization is pending.
- SQLite stores a local SHA-256 baseline and remote identity/version in file metadata
  properties. Existing Windows metadata is preserved; no Windows baseline is guessed.
- Simultaneous file edits preserve the local version under a unique `(conflict …)`
  name and fetch the remote version at the original path. The preserved copy is
  uploaded on a subsequent scan. On first sync, existing local files with matching
  remote paths are handled conservatively as conflicts too.
- File/directory type conflicts and filename aliases pause reconciliation and appear
  as errors in drive settings. Resolve these names manually and sync again.
- Symlinks and special files are rejected. Case/Unicode aliases are conservatively
  rejected even on case-sensitive volumes. Executable bits, ACLs, resource forks,
  extended attributes, and hard-link identity are not synchronized.
- Renames currently converge as creation and deletion, so remote object identity
  and history are not retained across a local rename.
- Inaccessible roots and failed/partial directory scans never authorize deletion.
  Local directories are removed only when empty; remote directories are re-listed
  before soft deletion. The server API does not provide an atomic version-conditional
  delete, so a concurrent remote edit after the final check remains a release-review
  concern. Do not use this preview as the only copy of important files.
- Downloads use a same-directory temporary file, length validation, fsync, and rename.
  They check the local content precondition before starting and before replacing it.
  Uploads check the source again before marking it synchronized. These checks are
  optimistic; macOS file coordination and stronger concurrent-writer handling remain
  future work.
- App settings, inventory, and credentials retain the existing `~/.cloudreve` layout.
  Keychain credential migration is not included.
- macOS notifications and login launch use native Tauri plugins. Login launch should
  be enabled after installing the app at its final location. OAuth uses the bundled
  `cloudreve://` scheme; the authorizing app must remain running during the flow.

## Verification

```sh
cargo check -p cloudreve-desktop
cargo test -p cloudreve-api -p cloudreve-sync --lib
npm --prefix ui run build
```

The macOS integration test runs a temporary loopback HTTP service and a temporary
SQLite database; it does not access user accounts or cloud content. It exercises
initial download, remote update, concurrent-edit preservation, local and remote
deletion, an empty-file upload, and failure of the remote listing.

Before release, test real account authorization, each upload provider with nonempty
files, multi-client races, interrupted transfers, disk-full/permission failures,
sleep/wake, network recovery, upgrade/uninstall, and Windows CFAPI regressions.
