//! Gives the shared library its SONAME, so programs linked against it record `libvds.so.0`
//! and the Debian package can install it under that name.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        println!("cargo:rustc-cdylib-link-arg=-Wl,-soname,libvds.so.0");
    }
}
