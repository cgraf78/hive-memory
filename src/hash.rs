//! Stable hash formatting shared by persisted identifiers and cache keys.

use sha2::{Digest, Sha256};

const LOWER_HEX: &[u8; 16] = b"0123456789abcdef";

/// Return the lowercase hexadecimal SHA-256 digest of `contents`.
pub fn sha256_hex(contents: &[u8]) -> String {
    lower_hex(&Sha256::digest(contents))
}

/// Encode bytes as lowercase hexadecimal with two characters per byte.
pub fn lower_hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(LOWER_HEX[(byte >> 4) as usize] as char);
        encoded.push(LOWER_HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::{lower_hex, sha256_hex};

    #[test]
    fn lower_hex_preserves_leading_zeroes() {
        assert_eq!(lower_hex(&[0x00, 0x01, 0x0f, 0x10, 0xff]), "00010f10ff");
    }

    #[test]
    fn sha256_hex_matches_the_standard_empty_input_vector() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
