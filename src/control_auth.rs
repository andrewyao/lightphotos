// SPDX-License-Identifier: MIT OR Apache-2.0

//! The `--control` channel's sign-in, shared by the app (`shell/control/tcp.rs`)
//! and `lightphotos-mcp`. The token never crosses the socket: the app sends a
//! fresh nonce, and the client answers with an HMAC-SHA256 of
//! [`PREFIX`]`<nonce>` keyed by the token. Whatever listens on the port learns
//! only that MAC, which is no use on another connection.

use ring::hmac;

/// What the MAC covers before the nonce, so a MAC made here can't stand for
/// one made for some other use of the token.
pub const PREFIX: &str = "lightphotos-control:";

/// The longest nonce a client signs. The app sends 64 hex characters.
pub const MAX_NONCE: usize = 128;

fn key(token: &str) -> hmac::Key {
    hmac::Key::new(hmac::HMAC_SHA256, token.as_bytes())
}

fn message(nonce: &str) -> Vec<u8> {
    [PREFIX.as_bytes(), nonce.as_bytes()].concat()
}

/// The `mac` a client sends for `nonce`: lowercase hex.
pub fn mac_hex(token: &str, nonce: &str) -> String {
    let tag = hmac::sign(&key(token), &message(nonce));
    tag.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether `mac` (hex, either case) is the MAC of `nonce` under `token`.
/// The compare takes the same time wherever the MACs differ.
pub fn verify(token: &str, nonce: &str, mac: &str) -> bool {
    let Some(bytes) = from_hex(mac) else {
        return false;
    };
    hmac::verify(&key(token), &message(nonce), &bytes).is_ok()
}

/// Whether `nonce` is one a client should sign: hex, not empty, and short.
pub fn is_nonce(nonce: &str) -> bool {
    !nonce.is_empty() && nonce.len() <= MAX_NONCE && nonce.bytes().all(|b| b.is_ascii_hexdigit())
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
    fn a_mac_checks_only_for_its_token_and_nonce() {
        let mac = mac_hex(TOKEN, "00ff");
        assert_eq!(mac.len(), 64);
        assert!(verify(TOKEN, "00ff", &mac));
        assert!(verify(TOKEN, "00ff", &mac.to_uppercase()));
        assert!(!verify(TOKEN, "00fe", &mac), "another nonce");
        assert!(!verify("another-token-of-length", "00ff", &mac));
        assert!(!verify(TOKEN, "00ff", &mac[..62]), "cut short");
        assert!(!verify(TOKEN, "00ff", "zz"), "not hex");
        assert!(!verify(TOKEN, "00ff", "é"), "not ascii");
        assert!(!verify(TOKEN, "00ff", ""));
    }

    /// RFC 4231 test case 2, so another client can match it.
    #[test]
    fn the_mac_is_plain_hmac_sha256() {
        let tag = hmac::sign(&key("Jefe"), b"what do ya want for nothing?");
        let hex: String = tag.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // The README's openssl line gives the same:
        // printf 'lightphotos-control:ab' | openssl dgst -sha256 -hmac k
        assert_eq!(
            mac_hex("k", "ab"),
            "e8c778c1da228c488feda1236ea27d4218f7030095c9842b158c105c2db50adf"
        );
    }

    #[test]
    fn only_short_hex_is_a_nonce() {
        assert!(is_nonce("00ff"));
        assert!(!is_nonce(""));
        assert!(!is_nonce("xyz"));
        assert!(!is_nonce(&"a".repeat(MAX_NONCE + 1)));
    }
}
