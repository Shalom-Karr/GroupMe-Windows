# CLAUDE.md

Working notes for this repository. Everything here cost real debugging time at
least once; the point is that it costs it only once.

For GroupMe API behaviour see [`docs/groupme-api.md`](docs/groupme-api.md) —
that is the protocol reference and it is not duplicated here. This file is about
building, testing and shipping *this* app.

---

## The one that mattered most

**`reqwest` must use `native-tls`, never `rustls-tls`.**

`rustls-tls` validates against `webpki-roots` compiled into the binary and
ignores the Windows certificate store entirely. On any machine behind TLS
inspection — corporate proxy, security appliance, AV with HTTPS scanning — the
presented chain is issued by a private root that Windows trusts and rustls has
never heard of. Every request fails with
`invalid peer certificate: UnknownIssuer` while Edge, Chrome and PowerShell on
the same machine succeed.

v0.1.0 shipped this way and could not sync a single message. The failure is
total and silent: no API means the router never navigates to GroupMe, so no
token is captured, so nothing syncs, so the archive stays empty and the window
parks in offline mode looking like a UI bug.

`native-tls` uses schannel — the same TLS stack and trust store as every other
Windows application. A Windows desktop app should work wherever the user's
browser already works. The same applies to `tokio-tungstenite` for the
websocket.

**CI cannot catch this.** GitHub runners have no TLS inspection, and the test
suite talks to `wiremock` over plain HTTP. 144 tests passed while the app could
not reach the internet.

---

## Dispatching agents

**Large files: instruct them to use `Edit`, never `Write`.**
An agent asked to modify a 42 KB HTML file spent 20+ minutes generating a
full-file rewrite and produced nothing. The replacement, told explicitly to make
targeted `Edit` calls and never `Write` that file, finished in 15 edits.
Surgical edits are faster *and* safer — a regeneration can silently drop an
invariant that was never mentioned in the brief.

**Assign disjoint file ownership, explicitly.**
Name the files an agent owns and list the ones it must not touch. Parallel
agents editing the same file produce edits that apply cleanly and mean nothing.
When several agents need one contract, write the exact signatures into every
brief rather than letting each infer them.

**Brief them like cold colleagues who will not ask questions.**
They cannot see this conversation. Include the goal, the constraints, the
invariants that must survive, and what to report back. State what *not* to do as
plainly as what to do.

**Ask for verification, not assertion.**
"Confirm each constraint with evidence" produces materially better work than
"make sure it's correct". Agents that were told to prove no-network / read-only /
no-XSS went and grepped for it; the report was checkable.

**Expect them to find things the brief missed, and let them.**
Several of the best fixes this session came from agents pushing back: refusing to
fabricate a demo statistic, promoting a caveat above the download button,
noticing that an empty conversation would pin a sync loop at full request rate
forever. Leave room for that in the brief.

---

## Windows and PowerShell

**`Set-Content -Encoding utf8` writes a BOM on PowerShell 5.1**, and piping a
string into a native command prefixes one too. This corrupted the updater
signing secret (`Invalid symbol 239` — `0xEF` is the first BOM byte) and later
broke a `gh api` call with `Problems parsing JSON`.

Pass values as arguments instead of piping:
```powershell
gh secret set NAME --body $value          # good
$value | gh secret set NAME               # adds a BOM
gh api ... -f 'source[branch]=main'       # good
'{"source":...}' | gh api ... --input -   # adds a BOM
```
To write a file without a BOM: `[System.IO.File]::WriteAllText($p, $s, (New-Object System.Text.UTF8Encoding $false))`.

**Here-strings mangle quoting** when passing multi-line text to native commands.
Write the text to a file and use `-F`/`--body-file`, or use `printf` from Bash.

**PowerShell wraps native stderr as `NativeCommandError`**, so a command that
succeeded can look like it failed. Judge by exit code and by checking the actual
effect, not by whether stderr appeared.

**In Git Bash, `gh api /repos/...` gets path-rewritten** into
`C:/Program Files/Git/repos/...`. Omit the leading slash: `gh api repos/...`.

---

## Git

**A `git push` that exits 0 has not necessarily pushed.** This session reported
success while the network write never landed, and it was only caught by checking
the remote afterwards. Always verify:

```bash
L=$(git rev-parse HEAD); R=$(git ls-remote origin refs/heads/main | cut -f1)
[ "$L" = "$R" ] && echo CONFIRMED
```

**`Recv failure: Connection was aborted` on push** is intermittent here.
`git config --global http.version HTTP/1.1` helps. When git-over-HTTPS fails
persistently but `api.github.com` still works, push through the Git Data API
instead — blobs → tree → commit → update ref produces a genuine commit with the
correct parent. `tools/` has a script pattern for this. Note that blob creation
is eventually consistent: the tree call can return `422 not a valid blob` for a
blob uploaded seconds earlier, so retry with backoff.

---

## Tauri

**No comment keys in `tauri.conf.json`.** The schema rejects unknown fields, so
a `"//"` key fails the build script before any Rust compiles — and the error
surfaces as an opaque build-script failure. Put rationale in code comments or in
a capability `description`.

**Do not declare the main window in config *and* build it in `setup()`.**
Tauri creates config windows before the setup hook, so the second build returns
`WindowLabelAlreadyExists`, `setup()` returns `Err`, and the app does not start
at all. This window must be built in Rust because an initialization script (the
token capture) can only be attached at build time — so `app.windows` stays `[]`.

**`protocol-asset` must be enabled** for `convertFileSrc` to resolve. Without it
the `asset:` scheme handler is never registered, so every cached image is
downloaded to disk and can never be displayed — it fails gracefully and silently.

**A window can open with no webview in it, and `build()` still returns `Ok`.**
WebView2's user-data folder admits one owner at a time. If a previous instance's
`msedgewebview2.exe` processes outlive it — a force-kill, a crash, or an update
that relaunches before the old children exit — the next launch loses the race and
webview creation fails with `E_INVALIDARG` (`0x80070057`). wry logs
`failed to create webview` and carries on, so the app runs with a **blank white
window** while sync and the realtime socket work perfectly. Every signal except
the window says the app is healthy, which is indistinguishable from a UI bug.

It clears itself once the orphaned processes exit, so it presents as
intermittent. To tell it apart in one step, hold the binary constant and change
only the profile:

```bash
WEBVIEW2_USER_DATA_FOLDER=/some/fresh/dir ./app.exe   # loads => the old profile was held
```

The app now detects this: `inject.js` emits `groupme://page` lifecycle beacons and
a 15s watchdog reports the cause when none arrive. **A wrapper cannot debug its
own webview without that bridge** — a release build has no devtools, and a remote
page has no console to read.

**Log levels, not defaults.** `tauri_plugin_log::Builder::new()` logs `TRACE` for
every crate in the tree. The websocket stack alone emits a dozen lines per
keepalive: measured at 92% of a real log file, which rotated away the
connectivity transitions and media errors the log existed to record. A log that
destroys its own evidence is worse than no log. Set `.level(Info)`, keep this
crate at `Debug`, and pin the transport crates to `Warn`.

**Restore before show.** `show()` maps to `ShowWindow(SW_SHOW)`, which on a
*minimized* window leaves it iconic. Calling `show()` then `unminimize()`
restores nothing, and every route back to the app appears to do nothing:

```rust
window.unminimize();  // first
window.show();
window.set_focus();
```
Windows also refuses `SetForegroundWindow` to a process that does not own the
foreground, so a restored window can appear *behind* the active one — briefly
toggling always-on-top raises it without needing that privilege.

**Capabilities are per-window**, which is what keeps "read-only when offline"
structural: the offline reader's window is granted only the `archive_*` readers,
so it cannot send even if a future refactor forgets. Mutating commands live in a
separate module exposed to a different window. Note that app-defined commands
bypass the ACL in this project (no ACL manifest), so the enforcement is *which
window loads which page* — the capability files document intent.

**Remote origins genuinely cannot invoke app commands.** Verified against
Tauri 2.11.5: a capability without a `remote` key resolves to `ExecutionContext::Local`
only, and non-local origins are hard-rejected. `web.groupme.com` can emit and
listen to events but cannot read the archive. It *can* forge an event, so
anything an event triggers must be re-validated in Rust.

Use `emit_to("main", …)` rather than `emit`. Broadcast events reach the remote
GroupMe page, which holds `core:event:allow-listen`.

---

## Rust

**`watch::Sender::send` fails when every receiver is dropped — and does not
store the value.** A connectivity monitor whose constructor dropped its seed
receiver silently discarded every state change; `state()` returned the initial
value forever while the logic was perfectly correct. Use `send_if_modified`,
which always writes, notifies only on real change, and closes the
read-then-write race of a separate `borrow()` + `send()`.

**A DM is stored under the *other participant's* user id, not the `"{a}+{b}"`
thread key.** `upsert_chat` uses `other_user.id` as the primary key, so a DM's
conversation id is a bare number that looks exactly like a group id. Anything
that decides "group or DM?" by looking for a `+` is wrong for every DM the
archive holds. That mistake subscribed every DM to `/group/{user_id}`, a channel
the account does not own, and GroupMe answered
`/meta/subscribe: Access token authentication failed` — which correctly stops the
realtime worker, so live updates died on the first DM opened. Pass the kind
explicitly and refuse to default it.

**`unread_count` / `last_read_message_id` / `last_read_at` on group and chat
objects are always `null`.** They exist in the payload, which makes them look
usable. Read state has exactly one source: `GET /v4/read_receipts`, which returns
the whole map in one call (375 entries here). Receipts key DMs by the `+`-joined
thread key, so they need mapping onto the stored ids above.

**`/v4` is enveloped too.** Only the endpoints listed in `docs/groupme-api.md` §3
escape `{"meta":…,"response":…}`. Decoding the top level of an enveloped response
does not error — it silently yields empty, which for read receipts is
indistinguishable from "you have read nothing" and leaves every conversation
marked unread.

**`#[serde(default)]` does not handle explicit `null`.** It fires only when the
key is *absent*. A key present with `null` goes to the field's own `Deserialize`,
and `Vec`/`bool`/`i64`/`String` all reject it — failing the whole response.
GroupMe sends `"members": null` on 200 of 211 groups. Every non-`Option` field
pairs `#[serde(default)]` with a `null_as_default` deserializer.

**The archive lives behind a blocking `std::sync::Mutex`, accessed only inside
`spawn_blocking`.** The guard must never be alive across an `.await` — the
future stops being `Send` and `tauri::async_runtime::spawn` rejects it. There is
a compile-time `Send` assertion in the sync tests specifically to catch this.

**`%LOCALAPPDATA%`, not `%APPDATA%`.** `app_data_dir()` is the *roaming* profile,
which a domain-joined machine syncs to the server at logon. Use
`app_local_data_dir()` — this archive reaches multiple gigabytes.

---

## CI

**Pin the toolchain.** `dtolnay/rust-toolchain@stable` floats, so CI ran lints
the local clippy did not have and a genuinely clean tree failed. Pin an explicit
version and bump it deliberately.

**Do not gate on `-D warnings` across all of clippy.** A style suggestion
stopping a shipping build teaches people to bypass the gate rather than read it.
Deny `clippy::correctness`, `clippy::suspicious` and `clippy::perf`; let style
lints report.

**Run `cargo fmt` before pushing.** Work assembled from several agents will not
be rustfmt-clean, and the first CI run will die on formatting before it reaches a
compile.

**A Rust test binary carries no manifest, so comctl32 binds to v5.** Once the
library linked window-creation code (`proxy.rs`, the tray windows), the test
binary imported comctl32 *v6* functions — `TaskDialogIndirect`,
`SetWindowSubclass`, `DefSubclassProc` — that the v5 side-by-side assembly does
not export. A manifest-less test binary then fails to *launch* with
`STATUS_ENTRYPOINT_NOT_FOUND` (`0xc0000139`) before a single test runs, while the
app binary, which tauri gives a manifest, runs fine. The tell is exactly that
split: `cargo run` works, `cargo test` dies on load. It is easy to misread as
environmental and wave CI through — v0.5.0 shipped a commit message claiming just
that, and CI was red on the very machine class the app targets.

What it is *not*, so the days do not get spent there again: not the linker
(`rust-lld` reproduced it identically), not binary size or section count (six
sections; `debug=false` changed nothing), not a missing DLL (the DLL is present —
only the *function* is absent in v5, so the import table looks clean). The fast
diagnosis is to diff the failing test binary's imports against a known-good tag's
(`dumpbin /imports`, sorted, `comm`); the v6 window imports are the smoking gun.

The fix embeds the manifest for *every* target. cargo 1.96 rejects
`rustc-link-arg-tests`, and a second `/MANIFEST:EMBED` on top of tauri's winres
manifest collides (`LNK1123`), so: `WindowsAttributes::new_without_app_manifest()`
stops tauri embedding it (icon and version resource stay), and
`cargo::rustc-link-arg=/MANIFEST:EMBED` + `/MANIFESTINPUT:windows.manifest`
embeds our own — identical to tauri's default, just the Common-Controls v6
dependency — for the app bin and the test harnesses alike. One embed per binary.
Verify with `mt.exe -inputresource:app.exe;#1`: the manifest must still show
Common-Controls 6.0 and the `asInvoker` trustInfo link merges in.

**The `paths:` filter is an allowlist.** Only pushes touching `src-tauri/**`,
`package.json` or the workflow itself trigger a build — so documentation and
tooling changes never cut a release. It is evaluated across *all* commits in a
push, so keep docs commits separate from source commits.

**Updater artifacts come from the NSIS target only.** An MSI install can never
auto-update. NSIS is configured `installMode: "currentUser"` so it installs to
`%LOCALAPPDATA%` and updates need no admin — a per-machine install would need
UAC on every update, and the updater runs unattended and cannot answer a prompt.

---

## Testing and verification

**A green build proves it compiles and packages. It does not prove it runs.**
Four launch-affecting bugs this session were found by reading code or by running
the real binary, never by the suite: the TLS failure, the duplicate window
label, the missing `protocol-asset`, and the minimize dead-end. Install and run
the actual installer before claiming anything works end to end.

**A meta-test that scans its own source file will match itself.** The test
asserting no mutating command exists in `commands.rs` listed the forbidden
patterns as literals — in that same file — and failed on its own array. Scan
only the portion before `#[cfg(test)]`, and assemble the needles at runtime.

**Check the effect, not the exit code**, for anything crossing a network or a
process boundary. That applies to pushes, `gh` calls, and background commands.

---

## Privacy

**Never put real captured data in test fixtures.** Real names, user ids and
group ids from the traffic capture ended up in `model.rs` and `store.rs`
fixtures, including a comment claiming "values altered" when only the message
text had been. GroupMe user ids are stable and correlatable, so a public
`name → id` pair deanonymises that account anywhere else it appears.

Use synthetic values of the same shape: user `20000001`, group `10000001`,
message `170000000000000001`, membership `1000000001`.

**Capture output contains live credentials** — the access token, session
cookies, and, if sign-in happens during recording, the password field and 2FA
PIN, plus every message fetched. `tools/capture-out/` and
`tools/.chrome-profile/` are gitignored. Verify with `git check-ignore -v`
*before* writing anything sensitive, not after.

**The archive is unencrypted SQLite.** That is a deliberate, documented choice,
not an oversight — say so plainly in user-facing text rather than implying
protection that is not there. Windows Credential Manager protects the token
against *other users* and offline disk access, not against a process running as
the same user.

---

## Capturing API traffic

`tools/capture_api.py` drives Chrome behind a selenium-wire proxy and records
full request/response detail. `tools/analyze_ws.py` and
`tools/digest_capture.py` reduce the output to something reviewable.

**selenium-wire 5.1.0 is unmaintained and breaks on modern pyOpenSSL.**
pyOpenSSL 23.3 removed the legacy X509 API its vendored mitmproxy is built on;
four call sites fail (`Cert.altnames`, `create_ca`, `dummy_cert`,
`CertStore.create_store`) and the symptom is that *no page loads at all*, not an
obvious error. `capture_api.py` monkey-patches all four onto `cryptography` at
import.

**Websocket frames need CDP, not the proxy.** selenium-wire cannot see inside an
upgraded connection, and forcing Faye's long-poll fallback just yields nginx
`504`s from GroupMe's edge — their client always upgrades, so that path is not
really supported. `goog:loggingPrefs: {performance: ALL}` plus
`Network.webSocketFrameSent/Received` reads the frames directly.

**`traffic.jsonl` reaches 130 MB.** Never open it with a file-reading tool;
script over it.
