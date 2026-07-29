# GroupMe for Windows

**[Download](https://github.com/Shalom-Karr/groupme-windows/releases/latest)** ·
**[groupme-windows on the web](https://shalom-karr.github.io/groupme-windows/)**

A native Windows desktop app built with Tauri 2 and Rust. It wraps `https://web.groupme.com` in a WebView2 window while a background Rust worker archives groups, DMs, messages, and media into a local SQLite database. When the network is unavailable, the window switches to a bundled offline reader backed by that archive.

Three things make it worth installing:

- **Full-text search over your entire history.** GroupMe has no message search on any platform — its only search box filters the chat list by conversation *name*. This builds an FTS5 index over every message you have ever received.
- **Read everything offline**, including cached images and avatars.
- **A custom client UI** (since 0.2.0). After signing in once on the web client, a `Custom UI` button switches to the app's own interface: it reads from the local archive so it opens instantly, searches everything, updates live over GroupMe's realtime socket, and sends, edits, deletes, reacts and uploads through the API directly. A `Web UI` button switches back, and the app remembers which surface you used last.

The *offline reader* is **read-only**: you can read your whole history, you cannot send. That is deliberate rather than unfinished — a queue that silently fires messages hours later is a worse product than a disabled composer. The custom client keeps working offline for reading; its composer fails fast with a visible error rather than queueing.

Get the **`-setup.exe`**, not the `.msi` — only the NSIS build auto-updates, and it installs per-user so updates need no admin rights. See [Install the `-setup.exe`, not the `.msi`](#install-the--setupexe-not-the-msi) for why.

See [docs/architecture.md](docs/architecture.md) for how the three parts fit together, [docs/offline-behaviour.md](docs/offline-behaviour.md) for what works offline and what doesn't, and [docs/groupme-api.md](docs/groupme-api.md) for the GroupMe API reference this was built from — written entirely from proxied capture of the real client, and documenting a good deal that GroupMe's own docs omit.

---

## Requirements

### To run

- Windows 10 or Windows 11
- WebView2 runtime (the installer embeds a bootstrapper that installs it automatically)

### To build

- Rust 1.77 or later (set in `Cargo.toml` via `rust-version`)
- Node.js 20 or later
- MSVC Build Tools (the C++ workload — required by `rusqlite`'s bundled SQLite)
- `@tauri-apps/cli` (installed automatically by `npm install`)

---

## Build and run

```
npm install
npm run tauri dev        # development build with live reload
npm run tauri build      # release build + NSIS installer
```

The release installer lands at:

```
src-tauri\target\release\bundle\nsis\GroupMe_0.1.0_x64-setup.exe
```

The release binary itself is at `src-tauri\target\release\groupme-desktop.exe`. The NSIS installer is the distributable artifact; the raw binary has no runtime WebView2 bootstrapper.

An `.msi` is built alongside it at `src-tauri\target\release\bundle\msi\`.

### Install the `-setup.exe`, not the `.msi`

Both work, but they are not equivalent:

| | `-setup.exe` (NSIS) | `.msi` |
|---|---|---|
| Auto-updates | **Yes** | **No** |
| Needs admin | No | Yes |
| Installs to | `%LOCALAPPDATA%` | `Program Files` |

Tauri only produces updater artifacts from the NSIS target, so an MSI install
can never auto-update — it has to be replaced manually. The MSI also installs
per-machine, which means a UAC prompt every time; the updater runs unattended
and cannot answer one, so elevation and silent updates are mutually exclusive.

The NSIS build installs per-user precisely so updates need no admin rights at
all. The MSI exists for policy-managed or scripted deployment, where a central
system handles versioning anyway.

---

## How it works

Three parts share one SQLite file:

1. **Online** — the webview loads `https://web.groupme.com` unchanged. GroupMe's servers own the UI, so sending, uploads, reactions, and emoji all work without any effort on our part, and nothing breaks when they redeploy.

2. **Archive** — a background Rust worker calls `api.groupme.com/v3` directly with reqwest and writes to SQLite. It reads the API rather than the DOM: the API is a versioned contract, markup is not. The archiver is purely read-only against the API; no `POST`, `PUT`, or `DELETE` is ever issued.

3. **Offline** — when connectivity is lost, the window switches to a bundled local reader that renders directly from SQLite, with no network dependency.

Full design: [docs/architecture.md](docs/architecture.md). API reference: [docs/groupme-api.md](docs/groupme-api.md).

---

## Offline behaviour

When connectivity is lost the window switches to the local archive. You can browse all synced conversations, read full history, search message text, and view cached images. Sending, reacting, editing, and deleting are not available offline and there is no outbox — see [docs/offline-behaviour.md](docs/offline-behaviour.md) for the rationale.

---

## Where data lives

All application data is under:

```
%LOCALAPPDATA%\dev.shalomkarr.groupme\
```

| Path | Contents |
|---|---|
| `archive.db` | SQLite archive: conversations, messages, users, media index |
| `archive.db-wal` | SQLite WAL file (normal; do not delete while the app is running) |
| `media\` | Downloaded media bytes (images, video previews, avatars) |

Local, not roaming. `%APPDATA%` is the roaming profile, which a domain-joined
machine synchronises to the server at logon — and this archive reaches multiple
gigabytes.

To move or back up the archive, copy the whole directory with the app closed.
Copying `archive.db` alone while it is running gives you a database missing
whatever is still in the WAL.

---

## Privacy and security

**Access token.** The GroupMe `x-access-token` (a ~40-character bearer credential equivalent in power to the account password) is stored in Windows Credential Manager under the service name `dev.shalomkarr.groupme`. It is never written to the SQLite archive or any config file in plaintext. The archive stores only a SHA-256 fingerprint so the app can detect when a different account signs in.

**The archive is not encrypted.** `archive.db` and the `blobs\` directory are ordinary files under your Windows user profile. Anyone with access to your user account or disk can read every archived message and view every cached image. If this is a concern, use Windows BitLocker or equivalent full-disk encryption.

**The archiver is read-only.** No write operation (`POST`, `PUT`, `DELETE`) is ever issued against the GroupMe API. The app cannot send messages, delete content, or modify your account on your behalf. Offline read-only is structural, not a UI convention — there is no send command registered on the offline surface.

---

## Project layout

```
GroupMe Windows\
├── src-tauri\
│   ├── src\
│   │   ├── lib.rs             # Tauri builder, wiring, surface routing
│   │   ├── main.rs            # Binary entry point (hides console in release)
│   │   ├── api.rs             # GroupMe REST client: reads, writes, uploads
│   │   ├── realtime.rs        # Faye/Bayeux websocket, live message delivery
│   │   ├── commands.rs        # Read-only IPC surface (archive_*)
│   │   ├── client_commands.rs # Write IPC surface (client_*)
│   │   ├── sync.rs            # Background archive worker
│   │   ├── connectivity.rs    # Online/offline state machine
│   │   ├── model.rs           # Serde types for the GroupMe API wire format
│   │   ├── store.rs           # SQLite schema, migrations, all read/write paths
│   │   ├── tray.rs            # Tray icon, menu, sync-status panel
│   │   ├── updater.rs         # Update check, staged install on exit
│   │   └── token.rs           # Windows Credential Manager wrapper, fingerprinting
│   ├── capabilities\          # Per-window permission scopes
│   ├── frontend\
│   │   ├── index.html      # Connectivity router; opens the last-used surface
│   │   ├── inject.js       # Init script: token capture + "Custom UI" toggle
│   │   ├── client.html     # The custom client (reads archive, writes via API)
│   │   ├── offline.html    # Bundled offline reader (read-only)
│   │   └── status.html     # Sync-status panel opened from the tray
│   ├── icons\              # App icons for the installer and taskbar
│   ├── Cargo.toml
│   └── tauri.conf.json
├── docs\
│   ├── architecture.md
│   ├── groupme-api.md      # API reference from live capture
│   ├── offline-behaviour.md
│   └── schema.md
├── tools\
│   ├── capture_api.py      # Selenium-Wire proxy capture tool
│   └── digest_capture.py   # Reduces traffic.jsonl to a readable digest
├── package.json
├── LICENSE
└── README.md
```

---

## Licence

Copyright (c) 2026 Shalom Karr. **AGPL-3.0-only WITH the Commons Clause.** Source-available, NOT open source.

**Free for personal, educational, research, and other non-commercial use**, subject to the AGPL copyleft: any copy, modification, or work built on this code must be released under these same terms, including network-deployed modifications.

**Commercial and business use requires a separate paid licence from the author.** Each of the following requires one, whether or not the software itself is sold:

- Internal use by, or on behalf of, a for-profit company or any organisation carrying on commercial activity
- Deployment to employees, contractors, clients, or customers
- Bundling or shipping it with, or alongside, any paid product, service, or support offering
- Offering it as a hosted or managed service
- Use in consulting, agency, contracting, or managed-service work delivered to a third party

To request a commercial licence, open an issue at <https://github.com/Shalom-Karr/groupme-archive>.

**Unofficial client.** GroupMe is a trademark of Microsoft Corporation. This application is independently developed and is not endorsed by, sponsored by, or affiliated with GroupMe or Microsoft. Users remain bound by GroupMe's Terms of Service.

The full licence text is in [LICENSE](LICENSE).
