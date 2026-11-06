//! Points the linker (and the runtime rpath) at the libmpv dylib.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=MPV_LIB_DIR");
    // libmpv location: MPV_LIB_DIR override, else Homebrew (Apple Silicon, then Intel).
    let dir = std::env::var("MPV_LIB_DIR").ok().unwrap_or_else(|| {
        ["/opt/homebrew/lib", "/usr/local/lib"]
            .into_iter()
            .find(|d| std::path::Path::new(d).join("libmpv.dylib").exists())
            .unwrap_or("/opt/homebrew/lib")
            .to_string()
    });
    println!("cargo:rustc-link-search=native={dir}");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
}
