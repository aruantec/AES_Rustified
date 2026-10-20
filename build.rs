fn main() {
    // Embed the AES icon into the .exe (Explorer/file icon). Rust ships
    // llvm-rc, so no extra toolchain is needed on MSVC targets.
    #[cfg(windows)]
    embed_resource::compile("app.rc", embed_resource::NONE)
        .manifest_optional()
        .unwrap();

    // Gzip the emulation title databases — the raw JSONs embed ~14MB
    // into the exe (~2MB compressed). Sources stay canonical .json;
    // the gz always regenerates from them. Embedded via OUT_DIR and
    // inflated lazily on first lookup (emu_meta::db_json).
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("db");
    std::fs::create_dir_all(&out).unwrap();
    println!("cargo:rerun-if-changed=assets/database");
    for e in std::fs::read_dir("assets/database").unwrap() {
        let path = e.unwrap().path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        println!("cargo:rerun-if-changed={}", path.display());
        let data = std::fs::read(&path).unwrap();
        let gz = out.join(format!("{}.gz", path.file_name().unwrap().to_str().unwrap()));
        let mut enc =
            flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        std::io::Write::write_all(&mut enc, &data).unwrap();
        std::fs::write(gz, enc.finish().unwrap()).unwrap();
    }
}
