// Links the libretro core given as a static library: out/cores/lib<core>.a
// (scripts/build-cores.sh), core name from RETRO_CORE (default pcsx_rearmed);
// scripts/build-emulators.sh builds one retro-<core> binary per core.
/// Cores written in C++.
const CXX_CORES: &[&str] = &["gambatte", "mednafen_pce_fast", "mednafen_ngp", "mednafen_wswan", "stella2014", "fbalpha2012"];

fn main() {
    let core = std::env::var("RETRO_CORE").unwrap_or_else(|_| "pcsx_rearmed".into());
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../out/cores");
    println!("cargo:rustc-link-search=native={}", dir.display());
    println!("cargo:rustc-link-lib=static={core}");
    println!("cargo:rustc-env=RETRO_CORE_NAME={core}");
    // Code for PCSX-ReARMed's own hooks (core patches): only links with it.
    println!("cargo::rustc-check-cfg=cfg(core_pcsx)");
    if core == "pcsx_rearmed" {
        println!("cargo:rustc-cfg=core_pcsx");
    }
    // Cores written in C++ need its runtime (zig ships libc++).
    if CXX_CORES.contains(&core.as_str()) {
        println!("cargo:rustc-link-arg-bins=-lc++");
    }
    // malloc & co. for the whole program, ahead of musl's.
    if std::env::var("RETRO_MALLOC").as_deref() != Ok("musl") {
        println!("cargo:rustc-link-lib=static:+whole-archive=mimalloc");
    }
    println!("cargo:rerun-if-env-changed=RETRO_MALLOC");
    // MIPS-optimised memcpy/memset (cores/libc-mips), ahead of musl's.
    if std::env::var("RETRO_STRING").as_deref() != Ok("musl") {
        println!("cargo:rustc-link-lib=static:+whole-archive=mipsstring");
    }
    println!("cargo:rerun-if-env-changed=RETRO_STRING");
    println!("cargo:rerun-if-env-changed=RETRO_CORE");
    // Load high: PCSX-ReARMed's recompiler maps the PS1's address space at
    // 0 (RAM + mirrors 0-8 MiB, scratchpad/IO 0x1f800000, BIOS 0x1fc00000)
    // for direct memory access; the default 0x400000 base is in the way.
    // Only for that core: lld's MIPS thunks (non-PIC code calling the PIC
    // C++ runtime) encode their jump target without masking it to 26 bits,
    // so above 0x10000000 they come out as invalid instructions - the C++
    // cores died with SIGILL there.
    if core == "pcsx_rearmed" {
        println!("cargo:rustc-link-arg-bins=-Wl,--image-base=0x50000000");
    }
    println!("cargo:rerun-if-changed={}", dir.join(format!("lib{core}.a")).display());
}
