// SPDX-License-Identifier: MIT OR Apache-2.0

//! The `--control` channel's sign-in, shared by the app (`shell/control/tcp.rs`)
//! and `lightphotos-mcp`. The token never crosses the socket, and each side
//! proves it holds the token before the other trusts it:
//!
//! 1. The client sends `hello` with a fresh nonce of its own.
//! 2. The app answers with its own nonce and the [`Side::Server`] MAC of
//!    both. A program squatting on the port can't make it, so the client
//!    stops there and sends nothing a squatter could test guesses against.
//! 3. The client sends the [`Side::Client`] MAC of both, and the app
//!    checks it.
//!
//! Each MAC is HMAC-SHA256, keyed by the token, of
//! `<side prefix><client nonce>:<server nonce>`, as lowercase hex. The two
//! prefixes keep one side's MAC from standing in for the other's.

use ring::hmac;

/// Which side of the sign-in a MAC proves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// The app, answering `hello`.
    Server,
    /// The client, in `auth`.
    Client,
}

impl Side {
    /// What the MAC covers before the nonces.
    pub fn prefix(self) -> &'static str {
        match self {
            Side::Server => "lightphotos-control/server:",
            Side::Client => "lightphotos-control/client:",
        }
    }
}

/// The longest nonce either side accepts. Each side sends 64 hex characters.
pub const MAX_NONCE: usize = 128;

fn key(token: &str) -> hmac::Key {
    hmac::Key::new(hmac::HMAC_SHA256, token.as_bytes())
}

fn message(side: Side, client_nonce: &str, server_nonce: &str) -> Vec<u8> {
    format!("{}{client_nonce}:{server_nonce}", side.prefix()).into_bytes()
}

/// `side`'s MAC over the two nonces, as lowercase hex.
pub fn mac_hex(token: &str, side: Side, client_nonce: &str, server_nonce: &str) -> String {
    hex(hmac::sign(&key(token), &message(side, client_nonce, server_nonce)).as_ref())
}

/// Whether `mac` (hex, either case) is `side`'s MAC over the two nonces
/// under `token`. The compare takes the same time wherever the MACs differ.
pub fn verify(token: &str, side: Side, client_nonce: &str, server_nonce: &str, mac: &str) -> bool {
    let Some(bytes) = from_hex(mac) else {
        return false;
    };
    hmac::verify(
        &key(token),
        &message(side, client_nonce, server_nonce),
        &bytes,
    )
    .is_ok()
}

/// Whether `nonce` is one to sign: hex, not empty, and short.
pub fn is_nonce(nonce: &str) -> bool {
    !nonce.is_empty() && nonce.len() <= MAX_NONCE && nonce.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A fresh nonce: 32 bytes from the OS random source, as 64 hex characters.
pub fn new_nonce() -> Result<String, String> {
    random_hex::<32>()
}

/// `N` bytes from the OS random source, as lowercase hex.
pub fn random_hex<const N: usize>() -> Result<String, String> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|e| format!("the OS gave no random bytes: {e}"))?;
    Ok(hex(&bytes))
}

/// `bytes` as lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    // A SHA-256 MAC is 64 hex characters; refuse anything far longer.
    let (pairs, odd) = text.as_bytes().as_chunks::<2>();
    if !odd.is_empty() || text.len() > 128 {
        return None;
    }
    let digit = |b: u8| (b as char).to_digit(16);
    pairs
        .iter()
        .map(|&[hi, lo]| Some((digit(hi)? * 16 + digit(lo)?) as u8))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn a_mac_checks_only_for_its_token_side_and_nonces() {
        let mac = mac_hex(TOKEN, Side::Client, "00ff", "aa");
        assert_eq!(mac.len(), 64);
        let check = |token, side, c, s, mac: &str| verify(token, side, c, s, mac);
        assert!(check(TOKEN, Side::Client, "00ff", "aa", &mac));
        assert!(check(
            TOKEN,
            Side::Client,
            "00ff",
            "aa",
            &mac.to_uppercase()
        ));
        assert!(
            !check(TOKEN, Side::Server, "00ff", "aa", &mac),
            "the other side's"
        );
        assert!(
            !check(TOKEN, Side::Client, "00fe", "aa", &mac),
            "another client nonce"
        );
        assert!(
            !check(TOKEN, Side::Client, "00ff", "ab", &mac),
            "another server nonce"
        );
        assert!(
            !check(TOKEN, Side::Client, "00f", "faa", &mac),
            "the nonces split elsewhere"
        );
        assert!(!check(
            "another-token-of-length",
            Side::Client,
            "00ff",
            "aa",
            &mac
        ));
        assert!(
            !check(TOKEN, Side::Client, "00ff", "aa", &mac[..62]),
            "cut short"
        );
        assert!(!check(TOKEN, Side::Client, "00ff", "aa", "zz"), "not hex");
        assert!(!check(TOKEN, Side::Client, "00ff", "aa", "é"), "not ascii");
        assert!(!check(TOKEN, Side::Client, "00ff", "aa", ""));
    }

    /// RFC 4231 test case 2, so another client can match it.
    #[test]
    fn the_mac_is_plain_hmac_sha256() {
        let tag = hmac::sign(&key("Jefe"), b"what do ya want for nothing?");
        assert_eq!(
            hex(tag.as_ref()),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // The README's openssl line gives the same:
        // printf 'lightphotos-control/client:%s:%s' ab cd | openssl dgst -sha256 -hmac k
        assert_eq!(
            mac_hex("k", Side::Client, "ab", "cd"),
            "c61df5f94a7e3bf7b768996aa11190f2835f344b25aa9a7ed5e27d4ad6992065"
        );
    }

    #[test]
    fn only_short_hex_is_a_nonce() {
        assert!(is_nonce("00ff"));
        assert!(!is_nonce(""));
        assert!(!is_nonce("xyz"));
        assert!(!is_nonce(&"a".repeat(MAX_NONCE + 1)));
        let (a, b) = (new_nonce().unwrap(), new_nonce().unwrap());
        assert!(is_nonce(&a) && a.len() == 64, "{a}");
        assert_ne!(a, b);
    }
}
