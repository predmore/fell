//! IMAP modified UTF-7 (RFC 3501). Mailbox names on the wire stay encoded.
//! The store records the decoded name.

use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD};

pub(super) fn encode_modified_utf7(input: &str) -> String {
    let mut pending = Vec::new();
    let mut encoded = String::new();
    for ch in input.chars().chain(std::iter::once(' ')) {
        if !((' '..='~').contains(&ch)) {
            pending.extend(
                ch.encode_utf16(&mut [0; 2])
                    .iter()
                    .flat_map(|unit| unit.to_be_bytes()),
            );
            continue;
        }
        if !pending.is_empty() {
            encoded.push('&');
            encoded.push_str(&STANDARD_NO_PAD.encode(&pending).replace('/', ","));
            encoded.push('-');
            pending.clear();
        }
        encoded.push(ch);
        if ch == '&' {
            encoded.push('-');
        }
    }
    encoded.pop(); // Sentinel flushes the final non-ASCII run.
    encoded
}

pub(super) fn decode_modified_utf7(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'&' {
            let start = index;
            index += 1;
            while index < bytes.len() && bytes[index] != b'&' {
                index += 1;
            }
            out.push_str(&input[start..index]);
            continue;
        }
        if bytes.get(index + 1) == Some(&b'-') {
            out.push('&');
            index += 2;
            continue;
        }
        index += 1;
        let start = index;
        while index < bytes.len() && bytes[index] != b'-' {
            index += 1;
        }
        let encoded = &input[start..index];
        if index < bytes.len() {
            index += 1;
        }
        match decode_utf16_b64(encoded) {
            Some(text) => out.push_str(&text),
            None => {
                out.push('&');
                out.push_str(encoded);
            }
        }
    }
    out
}

fn decode_utf16_b64(encoded: &str) -> Option<String> {
    let bytes = b64_decode(&encoded.replace(',', "/"))?;
    if bytes.len() < 2 {
        return None;
    }
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_be_bytes(*pair))
        .collect();
    String::from_utf16(&units).ok()
}

fn b64_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf: u32 = 0;
    let mut bits = 0;
    for ch in input.chars() {
        if ch == '=' {
            break;
        }
        let value = match ch {
            'A'..='Z' => ch as u32 - 'A' as u32,
            'a'..='z' => ch as u32 - 'a' as u32 + 26,
            '0'..='9' => ch as u32 - '0' as u32 + 52,
            '+' => 62,
            '/' => 63,
            _ => return None,
        };
        buf = (buf << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_and_ampersand_pass_through() {
        assert_eq!(decode_modified_utf7("INBOX"), "INBOX");
        assert_eq!(decode_modified_utf7("Sent &- Drafts"), "Sent & Drafts");
    }

    #[test]
    fn rfc3501_smiling_face() {
        assert_eq!(decode_modified_utf7("&Jjo-"), "☺");
    }
}
