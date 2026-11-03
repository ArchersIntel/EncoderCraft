fn main() {
    // Cargo doesn't otherwise notice icon swaps, so the exe kept the old embedded icon.
    println!("cargo:rerun-if-changed=icons");
    println!("cargo:rerun-if-changed=tauri.conf.json");
    tauri_build::build()
}
