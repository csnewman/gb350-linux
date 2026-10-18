// Links the PS1 rasteriser (scripts/build-ps1gpu.sh -> out/ps1gpu/libps1gpu.a).
fn main() {
    let lib = concat!(env!("CARGO_MANIFEST_DIR"), "/../../out/ps1gpu/libps1gpu.a");
    println!("cargo:rerun-if-changed={lib}");
    println!("cargo:rustc-link-arg={lib}");
}
