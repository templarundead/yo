//! Build script.
//!
//! The only job here is to make cargo aware of the two embedded databases so
//! that touching them triggers a rebuild — the data itself is pulled into the
//! binary by `include_bytes!` inside `src/yo_core.rs`, never read at runtime.

use std::env;
use std::path::Path;

fn main() {
    let manifest = env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    for asset in ["assets/yox.dat", "assets/yo_regexpr.dat"] {
        let path = Path::new(&manifest).join(asset);
        if !path.exists() {
            // Fail loudly at build time instead of producing a binary whose
            // dictionary silently went missing.
            panic!("missing embedded asset: {}", path.display());
        }
        println!("cargo:rerun-if-changed={asset}");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
