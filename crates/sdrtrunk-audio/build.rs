fn main() {
    // mbelib (ISC licence, see mbelib/COPYRIGHT) is compiled in, so the Pi needs no extra package.
    let files = ["mbelib.c", "ecc.c", "imbe7200x4400.c", "imbe7100x4400.c", "ambe3600x2400.c", "ambe3600x2450.c"];
    let mut b = cc::Build::new();
    b.include("mbelib").warnings(false).opt_level(2).file("csrc/shim.c");
    for f in files { b.file(format!("mbelib/{f}")); }
    b.compile("mbe");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") { println!("cargo:rustc-link-lib=m"); }
    println!("cargo:rerun-if-changed=csrc/shim.c");
    println!("cargo:rerun-if-changed=mbelib");
}
