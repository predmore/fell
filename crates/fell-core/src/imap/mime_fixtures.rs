//! Received mail with attachments, for the part listing and fetch tests.
//!
//! Each fixture is the raw message plus the decoded bytes of every part a
//! reader can fetch, by IMAP section.

pub(crate) struct MimeFixture {
    pub raw: Vec<u8>,
    /// `(section, decoded bytes)` of each listed part.
    pub bytes: Vec<(&'static str, Vec<u8>)>,
}

pub(crate) fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for (index, chunk) in data.chunks(3).enumerate() {
        if index > 0 && index % 19 == 0 {
            out.push_str("\r\n");
        }
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (at, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if at <= chunk.len() {
                out.push(ALPHABET[((n >> shift) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Bytes that look like a PDF and contain every byte value.
pub(crate) fn pdf_bytes() -> Vec<u8> {
    let mut bytes = b"%PDF-1.7\n".to_vec();
    bytes.extend((0..=255u8).cycle().take(3000));
    bytes
}

pub(crate) fn png_bytes(seed: u8) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend((0..700u32).map(|n| (n as u8).wrapping_mul(seed)));
    bytes
}

fn headers(id: &str, subject: &str, content_type: &str) -> String {
    format!(
        "From: Ann <ann@example.com>\r\n\
         To: jon@example.com\r\n\
         Subject: {subject}\r\n\
         Message-ID: <{id}>\r\n\
         Date: Mon, 16 Nov 2026 12:00:00 +0000\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: {content_type}\r\n\r\n"
    )
}

/// multipart/mixed: a text body, a PDF (base64) and a CSV
/// (quoted-printable, with an `=` and a non-ASCII byte).
pub(crate) fn mixed_two_attachments(id: &str) -> MimeFixture {
    let pdf = pdf_bytes();
    let csv = "name,amount\r\nCaf\u{e9},12=4\r\n".as_bytes().to_vec();
    let raw = format!(
        "{}--mix\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\r\n\
         Two files attached.\r\n\
         --mix\r\n\
         Content-Type: application/pdf; name=\"report.pdf\"\r\n\
         Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
         Content-Transfer-Encoding: base64\r\n\r\n\
         {}\r\n\
         --mix\r\n\
         Content-Type: text/csv; charset=utf-8\r\n\
         Content-Disposition: attachment; filename=\"totals.csv\"\r\n\
         Content-Transfer-Encoding: quoted-printable\r\n\r\n\
         name,amount\r\n\
         Caf=C3=A9,12=3D4\r\n\
         \r\n\
         --mix--\r\n",
        headers(id, "Two files", "multipart/mixed; boundary=\"mix\""),
        base64(&pdf),
    );
    MimeFixture {
        raw: raw.into_bytes(),
        bytes: vec![("2", pdf), ("3", csv)],
    }
}

/// multipart/related: alternative text and HTML that shows `logo@x` by
/// `cid:`, the logo, and an image the HTML never names.
pub(crate) fn related_cid_images(id: &str) -> MimeFixture {
    let logo = png_bytes(3);
    let stray = png_bytes(7);
    let raw = format!(
        "{}--rel\r\n\
         Content-Type: multipart/alternative; boundary=\"alt\"\r\n\r\n\
         --alt\r\n\
         Content-Type: text/plain\r\n\r\n\
         Our logo\r\n\
         --alt\r\n\
         Content-Type: text/html\r\n\r\n\
         <p>Our logo</p><img src=\"CID:Logo%40x\" alt=\"\">\r\n\
         --alt--\r\n\
         \r\n\
         --rel\r\n\
         Content-Type: image/png\r\n\
         Content-ID: <logo@x>\r\n\
         Content-Transfer-Encoding: base64\r\n\r\n\
         {}\r\n\
         --rel\r\n\
         Content-Type: image/png; name=\"stray.png\"\r\n\
         Content-ID: <stray@x>\r\n\
         Content-Transfer-Encoding: base64\r\n\r\n\
         {}\r\n\
         --rel--\r\n",
        headers(
            id,
            "Logo",
            "multipart/related; boundary=\"rel\"; type=\"multipart/alternative\""
        ),
        base64(&logo),
        base64(&stray),
    );
    MimeFixture {
        raw: raw.into_bytes(),
        bytes: vec![("2", logo), ("3", stray)],
    }
}

/// An attachment named with RFC 2231 (charset, percent-encoding, and a
/// continuation): `Ñandú 日本語.txt`.
pub(crate) fn rfc2231_name(id: &str) -> MimeFixture {
    let text = b"plain bytes\r\n".to_vec();
    let raw = format!(
        "{}--b\r\n\
         Content-Type: text/plain\r\n\r\n\
         see the file\r\n\
         --b\r\n\
         Content-Type: application/octet-stream\r\n\
         Content-Disposition: attachment;\r\n \
         filename*0*=UTF-8''%C3%91and%C3%BA%20;\r\n \
         filename*1*=%E6%97%A5%E6%9C%AC%E8%AA%9E.txt\r\n\r\n\
         plain bytes\r\n\
         \r\n\
         --b--\r\n",
        headers(id, "Name", "multipart/mixed; boundary=\"b\""),
    );
    MimeFixture {
        raw: raw.into_bytes(),
        bytes: vec![("2", text)],
    }
}

/// A forward as an attachment: message/rfc822 holding its own text and a
/// zip file.
pub(crate) fn nested_message(id: &str) -> MimeFixture {
    let zip = b"PK\x03\x04 inner zip bytes".to_vec();
    let inner = format!(
        "From: Bob <bob@example.com>\r\n\
         Subject: Original\r\n\
         Message-ID: <inner-{id}>\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: multipart/mixed; boundary=\"in\"\r\n\r\n\
         --in\r\n\
         Content-Type: text/plain\r\n\r\n\
         the original\r\n\
         --in\r\n\
         Content-Type: application/zip; name=\"inner.zip\"\r\n\
         Content-Transfer-Encoding: base64\r\n\r\n\
         {}\r\n\
         --in--",
        base64(&zip)
    );
    let raw = format!(
        "{}--out\r\n\
         Content-Type: text/plain\r\n\r\n\
         forwarding this\r\n\
         --out\r\n\
         Content-Type: message/rfc822; name=\"Original.eml\"\r\n\
         Content-Disposition: attachment\r\n\r\n\
         {inner}\r\n\
         --out--\r\n",
        headers(id, "Fwd: Original", "multipart/mixed; boundary=\"out\""),
    );
    MimeFixture {
        raw: raw.into_bytes(),
        bytes: vec![("2", inner.into_bytes()), ("2.2", zip)],
    }
}

/// A PGP/MIME signed message: its only extra part is `signature.asc`.
pub(crate) fn pgp_signed(id: &str) -> MimeFixture {
    let signature =
        b"-----BEGIN PGP SIGNATURE-----\r\n\r\niQEz\r\n-----END PGP SIGNATURE-----\r\n".to_vec();
    let raw = format!(
        "{}--sig\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\r\n\
         signed text\r\n\
         --sig\r\n\
         Content-Type: application/pgp-signature; name=\"signature.asc\"\r\n\
         Content-Disposition: attachment; filename=\"signature.asc\"\r\n\r\n\
         {}\r\n\
         --sig--\r\n",
        headers(
            id,
            "Signed",
            "multipart/signed; boundary=\"sig\"; micalg=pgp-sha256; protocol=\"application/pgp-signature\""
        ),
        String::from_utf8(signature.clone()).unwrap(),
    );
    MimeFixture {
        raw: raw.into_bytes(),
        bytes: vec![("2", signature)],
    }
}

/// One base64 attachment of `size` bytes after a short text part.
pub(crate) fn large_attachment(id: &str, size: usize) -> MimeFixture {
    let data: Vec<u8> = (0..size as u32)
        .map(|n| (n.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    let raw = format!(
        "{}--big\r\n\
         Content-Type: text/plain\r\n\r\n\
         a big file\r\n\
         --big\r\n\
         Content-Type: application/octet-stream; name=\"big.bin\"\r\n\
         Content-Disposition: attachment; filename=\"big.bin\"\r\n\
         Content-Transfer-Encoding: base64\r\n\r\n\
         {}\r\n\
         --big--\r\n",
        headers(id, "Big", "multipart/mixed; boundary=\"big\""),
        base64(&data),
    );
    MimeFixture {
        raw: raw.into_bytes(),
        bytes: vec![("2", data)],
    }
}

/// multipart/mixed: a text body and a Latin-1 `notes.txt` and `page.html`
/// (quoted-printable, non-ASCII). Their sizes are the bytes after transfer
/// decoding, not after a charset conversion to UTF-8.
pub(crate) fn latin1_text_attachments(id: &str) -> MimeFixture {
    let notes = b"Caf\xe9 au lait\r\n".to_vec();
    let page = b"<p>R\xe9sum\xe9</p>\r\n".to_vec();
    let raw = format!(
        "{}--l1\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\r\n\
         Notes attached.\r\n\
         --l1\r\n\
         Content-Type: text/plain; charset=iso-8859-1; name=\"notes.txt\"\r\n\
         Content-Transfer-Encoding: quoted-printable\r\n\r\n\
         Caf=E9 au lait\r\n\
         \r\n\
         --l1\r\n\
         Content-Type: text/html; charset=windows-1252; name=\"page.html\"\r\n\
         Content-Disposition: inline; filename=\"page.html\"\r\n\
         Content-Transfer-Encoding: quoted-printable\r\n\r\n\
         <p>R=E9sum=E9</p>\r\n\
         \r\n\
         --l1--\r\n",
        headers(id, "Latin-1 notes", "multipart/mixed; boundary=\"l1\""),
    );
    MimeFixture {
        raw: raw.into_bytes(),
        bytes: vec![("2", notes), ("3", page)],
    }
}
