// Compile the WPE embedder shim (engine/wpeengine.c) and link it against WPE WebKit. Device-only:
// WPE lives on Linux/aarch64, so on any other host this is a no-op and the Rust side stubs out.
//
// The link flags come from pkg-config (wpe-webkit-1.1 + wpebackend-fdo-1.0) plus libwayland-server
// for the wl_shm_buffer accessors. Build inside the arm64 Debian container that has the -dev
// packages (see the browser-spike memory); the resulting binary runs on the device against the
// /home/root/wpe-test rootfs.

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "linux" {
        return; // Mac/other: engine is stubbed; nothing to compile or link.
    }

    println!("cargo:rerun-if-changed=engine/wpeengine.c");
    println!("cargo:rerun-if-changed=engine/wpeengine.h");

    let lib = pkg_config::Config::new()
        .probe("wpe-webkit-1.1")
        .expect("pkg-config wpe-webkit-1.1 (install libwpewebkit-1.1-dev)");
    let fdo = pkg_config::Config::new()
        .probe("wpebackend-fdo-1.0")
        .expect("pkg-config wpebackend-fdo-1.0 (install libwpebackend-fdo-1.0-dev)");
    // libsoup-3.0: SoupMessageHeaders for the resource-load audit (BROWSER_NETLOG). WPE already
    // pulls it transitively, but probing explicitly guarantees the include path + link flag.
    let soup = pkg_config::Config::new()
        .probe("libsoup-3.0")
        .expect("pkg-config libsoup-3.0 (install libsoup-3.0-dev)");

    let mut build = cc::Build::new();
    build.file("engine/wpeengine.c").include("engine");
    for inc in lib.include_paths.iter().chain(fdo.include_paths.iter()).chain(soup.include_paths.iter()) {
        build.include(inc);
    }
    build.flag_if_supported("-Wno-unused-parameter");
    build.compile("wpeengine");

    // Emit the link flags AGAIN, after `build.compile` has emitted `-lwpeengine`. `probe()` above
    // already emitted these, but BEFORE our static lib — and static-link order requires the WPE/glib
    // libraries to follow the object that references them (`libwpeengine.a`), or the linker drops
    // the undefined symbols. This trailing re-emission fixes that order; it is NOT redundant.
    for l in lib.libs.iter().chain(fdo.libs.iter()).chain(soup.libs.iter()) {
        println!("cargo:rustc-link-lib={l}");
    }
    for p in lib.link_paths.iter().chain(fdo.link_paths.iter()).chain(soup.link_paths.iter()) {
        println!("cargo:rustc-link-search=native={}", p.display());
    }
    println!("cargo:rustc-link-lib=wayland-server");
    // Re-probe if the pkg-config search path changes (e.g. a different sysroot in the container).
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");

    // Bake the runtime lib dir into the binary as an RPATH so the loader finds the WPE libs (and
    // their transitive deps — all live in this one dir) WITHOUT LD_LIBRARY_PATH or the /usr/lib
    // symlink. This matters for two things: (1) `selfupdate` smoke-tests every new binary with a bare
    // `--version`, which would otherwise fail to even load libWPEWebKit and roll back the whole
    // update; (2) the binary keeps working right after an OS update wipes the /usr/lib symlink,
    // before the launch wrapper re-creates it. --disable-new-dtags emits DT_RPATH (not DT_RUNPATH) so
    // it also covers the WPE libs' own inter-dependencies. Path is the fixed install location; the
    // launch wrapper still sources env.sh for the other runtime vars (WEBKIT_EXEC_PATH, EGL, fonts…).
    println!("cargo:rustc-link-arg-bins=-Wl,--disable-new-dtags");
    println!(
        "cargo:rustc-link-arg-bins=-Wl,-rpath,/home/root/secondbrain/wpe/usr/lib/aarch64-linux-gnu"
    );
}
