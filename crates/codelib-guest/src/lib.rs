//! The prebuilt codelib guest component.
//!
//! This crate is data: [`COMPONENT`] is StarlingMonkey (SpiderMonkey compiled
//! to WebAssembly) plus a small JavaScript wrapper, packaged as a component
//! that implements the `codelib:runtime/script` world. It is built from the
//! `guest/` directory of the repository with Node tooling (`guest/build.sh`)
//! and checked in, so depending on codelib needs no JavaScript toolchain.
//!
//! The build is not byte-reproducible, so the artifact is pinned by
//! [`SHA256`]; a test checks that the two files agree.

/// The component binary.
pub const COMPONENT: &[u8] = include_bytes!("../codelib-guest.wasm");

/// Lowercase hex sha256 of [`COMPONENT`].
pub const SHA256: &str = include_str!("../codelib-guest.wasm.sha256").trim_ascii();

/// The WIT world [`COMPONENT`] implements.
pub const WORLD: &str = "codelib:runtime/script@0.1.0";

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    #[test]
    fn component_matches_its_pin() {
        let digest = Sha256::digest(super::COMPONENT);
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            super::SHA256,
            "rebuild the guest and commit both files together"
        );
    }

    #[test]
    fn component_is_a_component() {
        // Component binaries start with the wasm magic and the component layer version.
        assert_eq!(&super::COMPONENT[..8], b"\0asm\x0d\x00\x01\x00");
    }
}
