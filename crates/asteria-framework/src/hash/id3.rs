//! ID3v2 header parsing shared by format detection and the audio hashers.

/// Returns the total length of the ID3v2 tag whose 10-byte header is `head`:
/// the header itself, the syncsafe-encoded body size, and a 10-byte footer
/// when the footer flag is set. Only ID3v2.4 has a footer; in earlier versions
/// the same flag bit means something else and adds nothing.
/// `None` if `head` is not an ID3v2 header or a size byte has its high bit set.
pub(crate) fn id3v2_total_len(head: &[u8; 10]) -> Option<u64> {
    if &head[..3] != b"ID3" || head[6..10].iter().any(|b| b & 0x80 != 0) {
        return None;
    }
    let size = head[6..10]
        .iter()
        .fold(0u64, |acc, b| (acc << 7) | u64::from(*b));
    let footer = if head[3] >= 4 && head[5] & 0x10 != 0 {
        10
    } else {
        0
    };
    Some(10 + size + footer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_header() {
        assert_eq!(
            id3v2_total_len(b"ID3\x04\x00\x00\x00\x00\x00\x05"),
            Some(15)
        );
        assert_eq!(
            id3v2_total_len(b"ID3\x03\x00\x00\x00\x00\x01\x00"),
            Some(10 + 128)
        );
    }

    #[test]
    fn v23_footer_bit_adds_nothing() {
        assert_eq!(
            id3v2_total_len(b"ID3\x03\x00\x10\x00\x00\x00\x05"),
            Some(15)
        );
        assert_eq!(
            id3v2_total_len(b"ID3\x02\x00\x10\x00\x00\x00\x05"),
            Some(15)
        );
    }

    #[test]
    fn v24_footer_adds_ten() {
        assert_eq!(
            id3v2_total_len(b"ID3\x04\x00\x10\x00\x00\x00\x05"),
            Some(25)
        );
    }

    #[test]
    fn rejects_non_id3_and_non_syncsafe() {
        assert_eq!(id3v2_total_len(b"ID4\x04\x00\x00\x00\x00\x00\x05"), None);
        assert_eq!(id3v2_total_len(b"ID3\x04\x00\x00\x00\x00\x00\x85"), None);
        assert_eq!(id3v2_total_len(b"ID3\x04\x00\x00\x80\x00\x00\x00"), None);
    }
}
