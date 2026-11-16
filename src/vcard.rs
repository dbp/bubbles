//! vCard parsing shared by the contact sources (EDS, a folder of `.vcf` files).
//!
//! Only the properties the app uses are read: `UID`, `FN` (falling back to `N`
//! or `ORG`), `TEL`, `EMAIL` and `PHOTO`. Handles what real address books
//! emit, not just EDS's output: Apple's `item1.TEL` group prefixes,
//! lowercase property names and parameters, escaped text (`\,`), folded
//! lines, and vCard 4 `data:` photos.

use crate::store::{Contact, ContactAddress};

/// Try to read a `file://` URI from a `PHOTO;VALUE=URI` field and return the
/// file bytes.  Returns `None` if the URI is not a local file URI or cannot
/// be read — callers should degrade gracefully.
///
/// Only genuinely local file URIs are accepted:
/// `file:///absolute/path` (empty authority), `file://localhost/path`,
/// `file://127.0.0.1/path`, and `file://[::1]/path`.  Any other hostname
/// silently yields `None` — no panics, no contact-parse failure.
fn read_photo_from_file_uri(uri: &str) -> Option<Vec<u8>> {
    let rest = uri.strip_prefix("file://")?;

    let path: std::path::PathBuf = if rest.starts_with('/') {
        // "file:///absolute/path" — authority is empty, path is absolute.
        std::path::PathBuf::from(rest)
    } else {
        // Has an authority component (hostname).  Only localhost/loopback
        // is accepted; everything else is treated as a remote URI.
        let slash = rest.find('/')?;
        let host = &rest[..slash];
        let path = &rest[slash..];

        let is_local = host.is_empty()
            || host.eq_ignore_ascii_case("localhost")
            || host == "127.0.0.1"
            || host == "[::1]"
            || host == "::1";
        if !is_local {
            return None;
        }
        std::path::PathBuf::from(path)
    };
    std::fs::read(path).ok()
}

/// Parse every vCard in `text`, which may hold several
/// `BEGIN:VCARD … END:VCARD` blocks (as `.vcf` exports do). Cards that fail to
/// parse (no `UID`) are skipped.
pub(crate) fn parse_vcards(text: &str) -> Vec<Contact> {
    let mut contacts = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("BEGIN:VCARD") {
            current = Some(String::new());
        }
        if let Some(card) = current.as_mut() {
            card.push_str(line);
            card.push('\n');
        }
        if trimmed.eq_ignore_ascii_case("END:VCARD") {
            if let Some(card) = current.take() {
                contacts.extend(parse_vcard(&card));
            }
        }
    }
    contacts
}

/// Parse one vCard (3.0 or 4.0) into a [`Contact`].
///
/// Returns `None` if the vCard lacks a `UID` property (every EDS contact
/// should have one, but we defend against malformed data).
pub(crate) fn parse_vcard(vcard: &str) -> Option<Contact> {
    let unfolded = unfold_vcard(vcard);

    let mut uid: Option<String> = None;
    let mut display_name: Option<String> = None;
    let mut structured_name: Option<String> = None;
    let mut org: Option<String> = None;
    let mut avatar: Option<Vec<u8>> = None;
    let mut addresses: Vec<ContactAddress> = Vec::new();

    for line in unfolded.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Split header (name + params) from value at the first colon. A line
        // without one is malformed; skip it rather than drop the whole card.
        let Some(colon) = line.find(':') else { continue };
        let header = &line[..colon];
        let value = &line[colon + 1..];

        // Property name is everything before the first semicolon, minus any
        // group prefix (Apple writes `item1.EMAIL`). Names are case-insensitive.
        let name = header.split(';').next().unwrap_or(header);
        let name = name.rsplit('.').next().unwrap_or(name).to_ascii_uppercase();
        let params = header.to_ascii_uppercase();

        match name.as_str() {
            "FN" => {
                let v = unescape_text(value);
                if !v.trim().is_empty() {
                    display_name = Some(v);
                }
            }
            "N" => structured_name = name_from_n(value),
            "ORG" => {
                let first = split_unescaped(value, ';').into_iter().next().unwrap_or_default();
                if !first.trim().is_empty() {
                    org = Some(first);
                }
            }
            "UID" => {
                uid = Some(value.to_owned());
            }
            "TEL" => {
                let number = strip_scheme(value, "tel:");
                if !number.is_empty() {
                    addresses.push(ContactAddress {
                        value: format!("tel:{number}"),
                        kind: "phone".into(),
                    });
                }
            }
            "EMAIL" => {
                let email = strip_scheme(value, "mailto:");
                if !email.is_empty() {
                    addresses.push(ContactAddress {
                        value: format!("mailto:{email}"),
                        kind: "email".into(),
                    });
                }
            }
            "PHOTO" => {
                if let Some(bytes) = photo_bytes(&params, value) {
                    avatar = Some(bytes);
                }
            }
            _ => {}
        }
    }

    // Every contact must have at least a UID.
    Some(Contact {
        uid: uid?,
        display_name: display_name.or(structured_name).or(org).unwrap_or_default(),
        avatar,
        addresses,
    })
}

/// Decode a `PHOTO` value: vCard 3.0 inline base64 (`ENCODING=b`), a vCard 4
/// `data:` URI, or a local `file://` URI.
fn photo_bytes(params: &str, value: &str) -> Option<Vec<u8>> {
    let base64 = if params.contains("ENCODING=B") {
        Some(value)
    } else if let Some(data) = value.strip_prefix("data:") {
        data.split_once(";base64,").map(|(_, b64)| b64)
    } else if params.contains("VALUE=URI") {
        return read_photo_from_file_uri(value);
    } else {
        None
    }?;
    let decoded = glib::base64_decode(base64);
    (!decoded.is_empty()).then(|| decoded.to_vec())
}

/// `TEL;VALUE=uri:tel:+1…` and similar carry the scheme in the value already.
fn strip_scheme<'a>(value: &'a str, scheme: &str) -> &'a str {
    let value = value.trim();
    match value.get(..scheme.len()) {
        Some(prefix) if prefix.eq_ignore_ascii_case(scheme) => &value[scheme.len()..],
        _ => value,
    }
}

/// `N:Family;Given;Middle;Prefix;Suffix` → "Prefix Given Middle Family Suffix".
fn name_from_n(value: &str) -> Option<String> {
    let parts = split_unescaped(value, ';');
    let get = |i: usize| parts.get(i).map(|s| s.trim()).unwrap_or("");
    let ordered = [get(3), get(1), get(2), get(0), get(4)];
    let joined = ordered.iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" ");
    (!joined.is_empty()).then_some(joined)
}

/// Split a structured value on unescaped `sep`, unescaping each component.
fn split_unescaped(value: &str, sep: char) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                parts.last_mut().unwrap().push('\\');
                parts.last_mut().unwrap().push(next);
            }
        } else if c == sep {
            parts.push(String::new());
        } else {
            parts.last_mut().unwrap().push(c);
        }
    }
    parts.iter().map(|p| unescape_text(p)).collect()
}

/// Undo vCard text escaping: `\\`, `\,`, `\;`, `\n` / `\N`.
fn unescape_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') | Some('N') => out.push('\n'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// Unfold continuation lines in a vCard string.
///
/// A line beginning with a single space (or tab) is a continuation of the
/// previous line — strip the leading whitespace and append.
fn unfold_vcard(vcard: &str) -> String {
    let mut result = String::new();
    let mut carry = String::new();

    for line in vcard.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            // Continuation: strip leading whitespace, append to previous line.
            carry.push_str(line.trim_start());
        } else {
            // Flush the carried line first.
            if !carry.is_empty() {
                if !result.is_empty() {
                    result.push('\n');
                }
                result.push_str(&carry);
            }
            carry = line.to_owned();
        }
    }

    // Flush the final carried line.
    if !carry.is_empty() {
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(&carry);
    }

    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn addresses(c: &Contact) -> Vec<&str> {
        c.addresses.iter().map(|a| a.value.as_str()).collect()
    }

    /// Parse a vCard whose PHOTO is a `VALUE=URI` pointing to a local file.
    /// The parser must read the file and set `avatar` to its bytes.
    #[test]
    fn parse_vcard_reads_photo_from_file_uri() {
        // Strict per-test isolation: fresh temp dir for the image file.
        let dir = TempDir::new().unwrap();
        let avatar_path = dir.path().join("avatar.png");
        let image_bytes: Vec<u8> = b"\x89PNG\r\n\x1a\n".to_vec(); // valid PNG header
        fs::write(&avatar_path, &image_bytes).unwrap();

        // Construct a vCard 3.0 with a PHOTO;VALUE=URI:file://... line.
        let uri = format!("file://{}", avatar_path.display());
        let vcard = format!(
            "\
BEGIN:VCARD
VERSION:3.0
FN:Test Contact
UID:test-uid-42
PHOTO;VALUE=URI:{}
END:VCARD",
            uri
        );

        let contact = parse_vcard(&vcard).expect("vCard should parse as Contact");

        assert!(
            contact.avatar.is_some(),
            "expected avatar to be read from PHOTO;VALUE=URI file://…, got None"
        );
        assert_eq!(
            contact.avatar.unwrap(),
            image_bytes,
            "avatar bytes must match the content of the PHOTO file"
        );
    }

    /// Regression: a non-local host in the file URI must NOT be treated as
    /// local — the parser silently returns None rather than reading a local
    /// path derived from a remote hostname.
    #[test]
    fn read_photo_rejects_non_local_host() {
        // file:// remote-host/path — must NOT produce avatar bytes.
        let vcard = "\
BEGIN:VCARD
VERSION:3.0
FN:Remote Contact
UID:test-uid-remote-99
PHOTO;VALUE=URI:file://some-remote-host/etc/passwd
END:VCARD";

        let contact = parse_vcard(vcard).expect("vCard should parse as Contact");
        assert!(
            contact.avatar.is_none(),
            "expected no avatar for file:// with non-local host, got Some"
        );
    }

    /// file://localhost/path is genuinely local and SHOULD be read.
    #[test]
    fn read_photo_accepts_localhost_host() {
        let dir = TempDir::new().unwrap();
        let avatar_path = dir.path().join("avatar_localhost.png");
        let image_bytes: Vec<u8> = b"\x89PNG\r\n\x1a\n".to_vec();
        fs::write(&avatar_path, &image_bytes).unwrap();

        // Use a file://localhost/... URI.
        let uri = format!("file://localhost{}", avatar_path.display());
        let vcard = format!(
            "\
BEGIN:VCARD
VERSION:3.0
FN:Localhost Contact
UID:test-uid-localhost-42
PHOTO;VALUE=URI:{}
END:VCARD",
            uri
        );

        let contact = parse_vcard(&vcard).expect("vCard should parse as Contact");
        assert!(
            contact.avatar.is_some(),
            "expected avatar for file://localhost/…, got None"
        );
        assert_eq!(
            contact.avatar.unwrap(),
            image_bytes,
            "avatar bytes must match the content of the PHOTO file for localhost URI"
        );
    }

    /// file://127.0.0.1/path is also local and SHOULD be read.
    #[test]
    fn read_photo_accepts_loopback_ip() {
        let dir = TempDir::new().unwrap();
        let avatar_path = dir.path().join("avatar_loopback.png");
        let image_bytes: Vec<u8> = b"\x89PNG\r\n\x1a\n".to_vec();
        fs::write(&avatar_path, &image_bytes).unwrap();

        let uri = format!("file://127.0.0.1{}", avatar_path.display());
        let vcard = format!(
            "\
BEGIN:VCARD
VERSION:3.0
FN:Loopback Contact
UID:test-uid-loopback-42
PHOTO;VALUE=URI:{}
END:VCARD",
            uri
        );

        let contact = parse_vcard(&vcard).expect("vCard should parse as Contact");
        assert!(
            contact.avatar.is_some(),
            "expected avatar for file://127.0.0.1/…, got None"
        );
        assert_eq!(
            contact.avatar.unwrap(),
            image_bytes,
            "avatar bytes must match for loopback URI"
        );
    }

    /// Apple's exports and iCloud CardDAV group properties as `item1.EMAIL`,
    /// with lowercase parameters. Those must still be read.
    #[test]
    fn apple_grouped_properties_are_read() {
        let vcard = "\
BEGIN:VCARD\r
VERSION:3.0\r
N:Appleseed;Johnny;;;\r
FN:Johnny Appleseed\r
item1.EMAIL;type=INTERNET;type=pref:johnny@example.com\r
item1.X-ABLabel:_$!<Other>!$_\r
item2.TEL;type=pref:+1 (555) 010-0000\r
UID:apple-1\r
END:VCARD\r
";
        let c = parse_vcard(vcard).unwrap();
        assert_eq!(c.display_name, "Johnny Appleseed");
        assert_eq!(addresses(&c), ["mailto:johnny@example.com", "tel:+1 (555) 010-0000"]);
    }

    #[test]
    fn lowercase_property_names_are_read() {
        let c = parse_vcard("begin:vcard\nfn:Low Case\ntel:555\nuid:lc\nend:vcard").unwrap();
        assert_eq!(c.display_name, "Low Case");
        assert_eq!(addresses(&c), ["tel:555"]);
    }

    #[test]
    fn escaped_display_name_is_unescaped() {
        let c = parse_vcard("BEGIN:VCARD\nFN:Smith\\, Jane\\; Esq.\nUID:esc\nEND:VCARD").unwrap();
        assert_eq!(c.display_name, "Smith, Jane; Esq.");
    }

    #[test]
    fn missing_fn_falls_back_to_n_then_org() {
        let n = parse_vcard("BEGIN:VCARD\nN:Doe;Jane;Q;Dr.;\nUID:n\nEND:VCARD").unwrap();
        assert_eq!(n.display_name, "Dr. Jane Q Doe");
        let org = parse_vcard("BEGIN:VCARD\nORG:Acme\\, Inc.;Sales\nUID:o\nEND:VCARD").unwrap();
        assert_eq!(org.display_name, "Acme, Inc.");
    }

    #[test]
    fn uri_valued_tel_is_not_double_prefixed() {
        let c = parse_vcard("BEGIN:VCARD\nTEL;VALUE=uri:tel:+15550100\nUID:u\nEND:VCARD").unwrap();
        assert_eq!(addresses(&c), ["tel:+15550100"]);
    }

    #[test]
    fn vcard4_data_uri_photo_is_decoded() {
        let c = parse_vcard("BEGIN:VCARD\nUID:p\nPHOTO:data:image/png;base64,iVBORw0KGgo=\nEND:VCARD")
            .unwrap();
        assert_eq!(c.avatar.unwrap(), b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn malformed_line_does_not_drop_the_card() {
        let c = parse_vcard("BEGIN:VCARD\nFN:Still Here\nthis line has no colon\nUID:m\nEND:VCARD")
            .unwrap();
        assert_eq!(c.display_name, "Still Here");
    }

    #[test]
    fn multi_card_file_yields_every_card() {
        let text = "\
BEGIN:VCARD
FN:One
UID:1
END:VCARD
BEGIN:VCARD
FN:No uid, skipped
END:VCARD
BEGIN:VCARD
FN:Two
UID:2
END:VCARD
";
        let names: Vec<String> = parse_vcards(text).into_iter().map(|c| c.display_name).collect();
        assert_eq!(names, ["One", "Two"]);
    }

    /// Cards exactly as iccontacts writes them (its demo contacts, CRLF,
    /// folded lines, Apple-style custom labels, an inline photo).
    #[test]
    fn reads_iccontacts_output() {
        let contacts = parse_vcards(include_str!("fixtures/iccontacts-demo.vcf"));
        assert_eq!(contacts.len(), 8);
        let by_uid = |uid: &str| contacts.iter().find(|c| c.uid == uid).unwrap();

        let ada = by_uid("demo-1");
        assert_eq!(ada.display_name, "Ada Lovelace");
        assert_eq!(addresses(ada), ["tel:+44 20 7946 0101", "mailto:ada@example.com"]);

        let grace = by_uid("demo-2");
        assert_eq!(grace.display_name, "Rear Adm. Grace Hopper");
        assert_eq!(grace.avatar.as_deref().map(|a| &a[..4]), Some(&b"\x89PNG"[..]));
        assert_eq!(addresses(grace).len(), 3);

        assert_eq!(by_uid("demo-3").display_name, "Corner Bakery");
        assert_eq!(addresses(by_uid("demo-4")), ["tel:+44 161 496 0105", "mailto:alan@example.net"]);
        assert_eq!(by_uid("demo-7").display_name, "someone@example.com");
    }
}
