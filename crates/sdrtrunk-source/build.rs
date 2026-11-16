fn main() {
    // Distros ship only libairspy.so.0 without the -dev package, so link it verbatim. Airspy support is Linux only.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        println!("cargo:rustc-link-lib=dylib:+verbatim=libairspy.so.0");
    }
}
