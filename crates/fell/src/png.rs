//! Small uncompressed PNGs so the compose demo can show an inline image.

pub fn chart_png() -> Vec<u8> {
    let width = 240u32;
    let height = 105u32;
    let bars = [18u32, 48, 78, 108, 150];
    let mut raw = Vec::with_capacity(((width * 3 + 1) * height) as usize);
    for y in 0..height {
        raw.push(0);
        for x in 0..width {
            let from_bottom = height - 1 - y;
            let (r, g, b) = bars
                .iter()
                .copied()
                .find(|left| x >= *left && x < left + 28 && from_bottom < bars_height(*left))
                .map(bar_color)
                .unwrap_or((26, 28, 36));
            raw.extend_from_slice(&[r, g, b]);
        }
    }
    let mut png = Vec::new();
    png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    let mut ihdr = Vec::new();
    ihdr.extend(width.to_be_bytes());
    ihdr.extend(height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &zlib_store(&raw));
    chunk(&mut png, b"IEND", &[]);
    png
}

fn bars_height(left: u32) -> u32 {
    match left {
        18 => 36,
        48 => 58,
        78 => 88,
        108 => 42,
        _ => 96,
    }
}

fn bar_color(left: u32) -> (u8, u8, u8) {
    match left {
        18 => (86, 166, 214),
        48 => (122, 162, 247),
        78 => (158, 206, 106),
        108 => (224, 175, 104),
        _ => (187, 154, 247),
    }
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend((data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc_bytes = kind.to_vec();
    crc_bytes.extend_from_slice(data);
    out.extend(crc32(&crc_bytes).to_be_bytes());
}

fn zlib_store(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut index = 0;
    if data.is_empty() {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while index < data.len() {
        let end = (index + 65535).min(data.len());
        let last = end == data.len();
        let block = &data[index..end];
        out.push(if last { 1 } else { 0 });
        let len = block.len() as u16;
        out.extend(len.to_le_bytes());
        out.extend((!len).to_le_bytes());
        out.extend_from_slice(block);
        index = end;
    }
    out.extend(adler32(data).to_be_bytes());
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let mut a = 1u32;
    let mut b = 0u32;
    for byte in data {
        a = (a + u32::from(*byte)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}
