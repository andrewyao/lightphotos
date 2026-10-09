// SPDX-License-Identifier: MIT OR Apache-2.0

//! `decode_probe`: a RAW decode test harness for the non-mac decode path. It
//! writes synthetic DNG fixtures (Linear DNG, Bayer, and DNG with a preview
//! sub-IFD) and checks rawler's decode against the known pixel values it
//! wrote. Extra CLI paths get a decode report for real camera files.
//!
//! macOS decodes RAW through ImageIO and has no rawler, so there it is a stub
//! that exits.

#[cfg(not(target_os = "macos"))]
mod nonmac;

fn main() {
    #[cfg(not(target_os = "macos"))]
    nonmac::main();

    #[cfg(target_os = "macos")]
    {
        eprintln!("decode_probe tests the non-mac RAW decode path; run it on Linux or Windows.");
        std::process::exit(1);
    }
}
