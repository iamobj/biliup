fn main() {
    pyo3_build_config::add_extension_module_link_args();
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-search=native=/Library/Developer/CommandLineTools/Library/Frameworks/Python3.framework/Versions/3.9/lib");
        println!("cargo:rustc-link-lib=python3.9");
        println!("cargo:rustc-link-arg=-Wl,-rpath,/Library/Developer/CommandLineTools/Library/Frameworks");
    }
}
