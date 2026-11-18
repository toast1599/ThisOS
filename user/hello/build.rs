//! Links the program at a fixed address, lld's default base for x86_64, so
//! that a loader maps it without relocating it.

fn main() {
    println!("cargo::rustc-link-arg-bins=--no-pie");
}
