use std::env;
use std::fs;
use std::path::PathBuf;

include!("src/plist.rs");

fn main() {
    println!("cargo::rerun-if-changed=Info.plist.template");
    println!("cargo::rerun-if-changed=src/plist.rs");

    let template = fs::read_to_string("Info.plist.template").expect("Info.plist.template");
    let plist = render(&template, env!("CARGO_PKG_VERSION"));

    let path = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("Info.plist");
    fs::write(&path, plist).expect("write Info.plist");

    println!(
        "cargo::rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}",
        path.display()
    );
}
