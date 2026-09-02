/// Encode bytes as lowercase hexadecimal without relying on digest-specific
/// formatting traits.
pub(crate) fn hex_lower(bytes: impl AsRef<[u8]>) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";

    let bytes = bytes.as_ref();
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        encoded.push(DIGITS[usize::from(byte >> 4)] as char);
        encoded.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    encoded
}

#[cfg(test)]
mod tests {
    #[test]
    fn encodes_lowercase_hex_with_leading_zeroes() {
        assert_eq!(super::hex_lower([0x00, 0x09, 0xab, 0xff]), "0009abff");
    }
}
