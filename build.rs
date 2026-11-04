//! Встраивает в vramdisk.exe значок, манифест (права администратора,
//! DPI per-monitor v2, Common Controls 6) и сведения о версии.

fn main() {
    for f in ["assets/vramdisk.rc", "assets/app.manifest", "assets/icon.ico"] {
        println!("cargo:rerun-if-changed={f}");
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let ver = |k: &str| std::env::var(k).unwrap_or_else(|_| "0".into());
    let (major, minor, patch) = (ver("CARGO_PKG_VERSION_MAJOR"), ver("CARGO_PKG_VERSION_MINOR"), ver("CARGO_PKG_VERSION_PATCH"));
    // Строковый макрос через -D не переживает вызов препроцессора windres
    // (кавычки съедает оболочка), поэтому версия идёт через заголовок.
    let out = std::env::var("OUT_DIR").expect("OUT_DIR");
    let header = format!(
        "#define VER_MAJOR {major}\n#define VER_MINOR {minor}\n#define VER_PATCH {patch}\n#define VER_STRING \"{major}.{minor}.{patch}\"\n"
    );
    std::fs::write(std::path::Path::new(&out).join("version.h"), header).expect("version.h");
    embed_resource::compile("assets/vramdisk.rc", embed_resource::NONE).manifest_required().unwrap();
}
