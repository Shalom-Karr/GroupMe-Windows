# GroupMe Desktop

**[Download](https://github.com/Shalom-Karr/groupme-windows/releases/latest)** ·
**[groupme-windows on the web](https://shalom-karr.github.io/groupme-windows/)**

A native desktop app for Windows and Linux, built with Tauri 2 and Rust. It wraps `https://web.groupme.com` in a webview window while a background Rust worker archives groups, DMs, messages, and media into a local SQLite database. When the network is unavailable, the window switches to a bundled offline reader backed by that archive.

> **Platform notes:** Windows is the primary target and has seen the most real-world use. Linux support was added in v0.14.0 and is less battle-tested — please report any issues.

Three things make it worth installing:

- **Full-text search over your entire history.** GroupMe has no message search on any platform — its only search box filters the chat list by conversation *name*. This builds an FTS5 index over every message you have ever received.
- **Read everything offline**, including cached images and avatars.
- **A custom client UI** (since 0.2.0). After signing in once on the web client, a `Custom UI` button switches to the app's own interface: it reads from the local archive so it opens instantly, searches everything, updates live over GroupMe's realtime socket, and sends, edits, deletes, reacts and uploads through the API directly. A `Web UI` button switches back, and the app remembers which surface you used last.

The *offline reader* is **read-only**: you can read your whole history, you cannot send. That is deliberate rather than unfinished — a queue that silently fires messages hours later is a worse product than a disabled composer. The custom client keeps working offline for reading; its composer fails fast with a visible error rather than queueing.

Get the **`-setup.exe`**, not the `.msi` — only the NSIS build auto-updates, and it installs per-user so updates need no admin rights. See [Install the `-setup.exe`, not the `.msi`](#install-the--setupexe-not-the-msi) for why.

See [docs/architecture.md](docs/architecture.md) for how the three parts fit together, [docs/offline-behaviour.md](docs/offline-behaviour.md) for what works offline and what doesn't, and [docs/groupme-api.md](docs/groupme-api.md) for the GroupMe API reference this was built from — written entirely from proxied capture of the real client, and documenting a good deal that GroupMe's own docs omit.

---

## Requirements

### To run — Windows

- Windows 10 or Windows 11
- WebView2 runtime (the NSIS installer embeds a bootstrapper that installs it automatically)

### To run — Linux

- A GTK3 desktop environment (GNOME, KDE, XFCE, etc.)
- `libwebkit2gtk-4.1-0` — WebKit engine (installed automatically by the `.deb`)
- `libayatana-appindicator3-1` — system tray icon (installed automatically by the `.deb`)
- **GNOME Keyring or KWallet** — required for sign-in to persist across restarts. Without a running Secret Service provider the app still works, but will ask you to sign in every launch.

### To build — Windows

- Rust 1.77 or later (set in `Cargo.toml` via `rust-version`)
- Node.js 20 or later
- MSVC Build Tools (the C++ workload — required by `rusqlite`'s bundled SQLite)
- `@tauri-apps/cli` (installed automatically by `npm install`)

### To build — Linux

- Rust 1.77 or later
- Node.js 20 or later
- System packages:
  ```
  sudo apt-get install -y libwebkit2gtk-4.1-dev libayatana-appindicator3-dev \
    librsvg2-dev patchelf build-essential libssl-dev libgtk-3-dev libxdo-dev
  ```
  (Fedora/rpm: equivalent `webkit2gtk4.1-devel`, `libayatana-appindicator-devel`)
- `@tauri-apps/cli` (installed automatically by `npm install`)

---

## Build and run

```
npm install
npm run tauri dev        # development build with live reload
npm run tauri build      # release build + installers
```

**Windows** release installers land at:

```
src-tauri\target\release\bundle\nsis\GroupMe_x.y.z_x64-setup.exe
src-tauri\target\release\bundle\msi\GroupMe_x.y.z_x64_en-US.msi
```

**Linux** release packages land at:

```
src-tauri/target/release/bundle/deb/groupme-desktop_x.y.z_amd64.deb
src-tauri/target/release/bundle/rpm/groupme-desktop-x.y.z-1.x86_64.rpm
src-tauri/target/release/bundle/appimage/groupme-desktop_x.y.z_amd64.AppImage
```

### Windows: install the `-setup.exe`, not the `.msi`

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

### Linux: which package to install

| Artifact | Use when |
|---|---|
| `.deb` | Ubuntu, Debian, and derivatives |
| `.rpm` | Fedora, openSUSE, RHEL derivatives |
| `.AppImage` | Any distribution; no install needed, just make it executable |

The `.deb` declares its runtime library dependencies so they install automatically.
For `.rpm` and `.AppImage`, install `libwebkit2gtk-4.1-0` and `libayatana-appindicator3-1`
(or their distro equivalents) manually if they are not already present.

**Auto-updates on Linux:** the updater is included and will work once the Linux
artifacts appear in a release's `latest.json`. Sign-in must persist (Secret Service
required) for the updater to run unattended.

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

**Windows:**

```
%LOCALAPPDATA%\dev.shalomkarr.groupme\
```

Local, not roaming. `%APPDATA%` is the roaming profile, which a domain-joined
machine synchronises to the server at logon — and this archive reaches multiple
gigabytes.

**Linux:**

```
~/.local/share/dev.shalomkarr.groupme/
```

| Path | Contents |
|---|---|
| `archive.db` | SQLite archive: conversations, messages, users, media index |
| `archive.db-wal` | SQLite WAL file (normal; do not delete while the app is running) |
| `media/` | Downloaded media bytes (images, video previews, avatars) |

To move or back up the archive, copy the whole directory with the app closed.
Copying `archive.db` alone while it is running gives you a database missing
whatever is still in the WAL.

---

## Privacy and security

**Access token.** The GroupMe `x-access-token` (a ~40-character bearer credential equivalent in power to the account password) is stored in the platform credential store — Windows Credential Manager on Windows, or the Secret Service (GNOME Keyring / KWallet) on Linux — under the service name `dev.shalomkarr.groupme`. It is never written to the SQLite archive or any config file in plaintext. The archive stores only a SHA-256 fingerprint so the app can detect when a different account signs in. On Linux, if no Secret Service provider is running, the token cannot be saved and sign-in will be required on every launch.

**The archive is not encrypted.** `archive.db` and the `media/` directory are ordinary files under your user profile. Anyone with access to your user account or disk can read every archived message and view every cached image. If this is a concern, use full-disk encryption (BitLocker on Windows, LUKS on Linux).

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
│   │   └── token.rs           # Platform credential store wrapper, fingerprinting
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
