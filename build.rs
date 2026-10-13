fn main() {
    // Embed the AES icon into the .exe (Explorer/file icon). Rust ships
    // llvm-rc, so no extra toolchain is needed on MSVC targets.
    #[cfg(windows)]
    embed_resource::compile("app.rc", embed_resource::NONE)
        .manifest_optional()
        .unwrap();
}
