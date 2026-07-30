fn main() {
    // Embed the Windows manifest through the linker for EVERY target — the app
    // binary and the test harnesses alike — rather than letting tauri embed it
    // into the app binary only.
    //
    // The library links window-creation code (proxy.rs, the tray windows) whose
    // wry dependency imports comctl32 v6 functions such as `TaskDialogIndirect`
    // and `SetWindowSubclass`. A test binary carries no manifest, so the loader
    // binds comctl32 to the v5 side-by-side assembly, which does not export those
    // functions, and the binary fails to launch at all with
    // STATUS_ENTRYPOINT_NOT_FOUND (0xc0000139) before a single test runs — while
    // the app binary, which gets tauri's manifest, runs fine. cargo 1.96 has no
    // `rustc-link-arg-tests` to scope a manifest to tests only, and a second
    // `/MANIFEST:EMBED` on top of tauri's winres manifest collides (LNK1123).
    //
    // So: tell tauri NOT to embed the manifest (`new_without_app_manifest` keeps
    // the icon and version resource, drops only the manifest), and embed our own
    // — identical to tauri's default, just the Common-Controls v6 dependency —
    // via the linker for all targets. One manifest, one embed per binary, tests
    // included.
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .windows_attributes(tauri_build::WindowsAttributes::new_without_app_manifest()),
    )
    .expect("failed to run tauri-build");

    // MSVC-linker flags; only emit them for that target env.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("windows.manifest");
        println!("cargo::rerun-if-changed=windows.manifest");
        println!("cargo::rustc-link-arg=/MANIFEST:EMBED");
        println!(
            "cargo::rustc-link-arg=/MANIFESTINPUT:{}",
            manifest.display()
        );
    }
}
