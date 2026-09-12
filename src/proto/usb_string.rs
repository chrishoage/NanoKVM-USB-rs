//! Parsing for CH9329 USB identification strings.
//!
//! These strings identify an already-open bridge. They cannot establish which serial
//! node is safe to open during discovery.

use std::fmt;

/// Which of the three strings a `GET_USB_STRING` transaction asks for.
///
/// The request payload is the single byte [`UsbStringKind::request_byte`], and the reply is
/// expected to echo it back as its first byte — which is what makes a mismatched reply detectable
/// at all, since the three replies are otherwise indistinguishable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsbStringKind {
    Manufacturer,
    Product,
    Serial,
}

impl UsbStringKind {
    /// All three, in the order a probe asks for them.
    pub const ALL: [UsbStringKind; 3] = [
        UsbStringKind::Manufacturer,
        UsbStringKind::Product,
        UsbStringKind::Serial,
    ];

    /// The request payload byte, which the reply must echo.
    pub fn request_byte(self) -> u8 {
        match self {
            UsbStringKind::Manufacturer => 0,
            UsbStringKind::Product => 1,
            UsbStringKind::Serial => 2,
        }
    }
}

impl fmt::Display for UsbStringKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            UsbStringKind::Manufacturer => "manufacturer",
            UsbStringKind::Product => "product",
            UsbStringKind::Serial => "serial",
        })
    }
}

/// The three strings a unit answers with.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsbStrings {
    pub manufacturer: String,
    pub product: String,
    pub serial: String,
}

impl UsbStrings {
    /// Store one string under the kind that was asked for.
    pub fn set(&mut self, kind: UsbStringKind, value: String) {
        match kind {
            UsbStringKind::Manufacturer => self.manufacturer = value,
            UsbStringKind::Product => self.product = value,
            UsbStringKind::Serial => self.serial = value,
        }
    }

    /// One string by kind.
    pub fn get(&self, kind: UsbStringKind) -> &str {
        match kind {
            UsbStringKind::Manufacturer => &self.manufacturer,
            UsbStringKind::Product => &self.product,
            UsbStringKind::Serial => &self.serial,
        }
    }
}

impl fmt::Display for UsbStrings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "manufacturer {:?}, product {:?}, serial {:?}",
            self.manufacturer, self.product, self.serial
        )
    }
}

/// Why a `GET_USB_STRING` reply could not be read as a string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsbStringError {
    /// No payload at all, so not even the type byte is there.
    Empty,
    /// The reply's first byte is not the one the request asked for, so this reply belongs to a
    /// different question and its bytes must not be filed under this one.
    WrongType { want: u8, got: u8 },
}

impl std::error::Error for UsbStringError {}

impl fmt::Display for UsbStringError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UsbStringError::Empty => write!(f, "empty payload, expected at least a type byte"),
            UsbStringError::WrongType { want, got } => write!(
                f,
                "reply is for string type {got:#04x}, asked for {want:#04x}"
            ),
        }
    }
}

/// Parse a `GET_USB_STRING` payload and escape it for display.
///
/// The measured form is `[type, len, ascii…]`. A matching length is consumed; otherwise
/// the remaining bytes are interpreted as the legacy `[type, ascii…]` form. Invalid
/// UTF-8 becomes `U+FFFD`, and control characters become `\u{…}` escapes.
pub fn parse_usb_string(kind: UsbStringKind, data: &[u8]) -> Result<String, UsbStringError> {
    let Some((&type_byte, rest)) = data.split_first() else {
        return Err(UsbStringError::Empty);
    };
    if type_byte != kind.request_byte() {
        return Err(UsbStringError::WrongType {
            want: kind.request_byte(),
            got: type_byte,
        });
    }
    let text = match rest.split_first() {
        Some((&len, tail)) if usize::from(len) == tail.len() => tail,
        _ => rest,
    };
    Ok(render(text))
}

/// Printable text for bytes the device chose, with everything unprintable escaped.
fn render(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_debug().collect::<Vec<char>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The datasheet shape: type, length, then that many bytes.
    #[test]
    fn a_reply_carrying_a_length_byte_is_read_without_it() {
        let data = [
            0x01, 0x0B, b'N', b'a', b'n', b'o', b'K', b'V', b'M', b'-', b'U', b'S', b'B',
        ];
        assert_eq!(
            parse_usb_string(UsbStringKind::Product, &data),
            Ok("NanoKVM-USB".to_string())
        );
    }

    /// Accept the legacy form without a length byte.
    #[test]
    fn a_reply_with_no_length_byte_is_read_as_text_after_the_type() {
        let data = [0x00, b'S', b'i', b'p', b'e', b'e', b'd'];
        assert_eq!(
            parse_usb_string(UsbStringKind::Manufacturer, &data),
            Ok("Sipeed".to_string())
        );
    }

    /// All string replies share command 0x8A; the type byte distinguishes the requested string.
    #[test]
    fn a_reply_for_another_string_type_is_rejected_rather_than_filed_under_this_one() {
        let data = [0x02, b'x'];
        assert_eq!(
            parse_usb_string(UsbStringKind::Product, &data),
            Err(UsbStringError::WrongType {
                want: 0x01,
                got: 0x02
            })
        );
    }

    #[test]
    fn an_empty_reply_is_an_error_and_a_type_only_reply_is_an_empty_string() {
        assert_eq!(
            parse_usb_string(UsbStringKind::Serial, &[]),
            Err(UsbStringError::Empty)
        );
        assert_eq!(
            parse_usb_string(UsbStringKind::Serial, &[0x02]),
            Ok(String::new())
        );
    }

    /// Escape control bytes so device output cannot control the terminal.
    #[test]
    fn unprintable_bytes_are_escaped_rather_than_passed_through() {
        let text = parse_usb_string(UsbStringKind::Serial, &[0x02, b'A', 0x07, 0xFF, b'B'])
            .expect("the type byte echoes, so this parses");
        assert!(text.starts_with('A') && text.ends_with('B'), "{text}");
        assert!(text.contains("\\u{7}"), "the bell must be escaped: {text}");
        assert!(
            text.contains('\u{FFFD}'),
            "a byte that is not UTF-8 must be replaced: {text}"
        );
    }

    /// A length byte that does not describe the rest of the payload is text, not a length.
    #[test]
    fn a_length_byte_that_disagrees_with_the_payload_is_kept_as_text() {
        assert_eq!(
            parse_usb_string(UsbStringKind::Manufacturer, &[0x00, 0x40, b'A']),
            Ok("@A".to_string())
        );
    }
}
