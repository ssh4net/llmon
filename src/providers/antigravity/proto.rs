//! A minimal protobuf wire-format reader. Antigravity stores its records as
//! protobuf blobs without a published schema, so the fields are read by number
//! and anything unknown is skipped. Every read is bounds-checked: a blob that
//! does not parse yields `None`, never a panic.

/// One field value of the wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Value<'a> {
    Varint(u64),
    /// A length-delimited field: a string, bytes, or a nested message.
    Bytes(&'a [u8]),
    /// A fixed-width field, kept raw.
    Fixed(u64),
}

/// A message: its fields in wire order.
#[derive(Debug, Clone, Default)]
pub(super) struct Message<'a> {
    fields: Vec<(u32, Value<'a>)>,
}

fn read_varint(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let mut result = 0_u64;
    let mut shift = 0_u32;
    loop {
        let byte = *bytes.get(*at)?;
        *at += 1;
        if shift >= 64 {
            return None;
        }
        result |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
    }
}

impl<'a> Message<'a> {
    /// Parses a whole message; `None` if it is cut off or has a field this
    /// reader cannot step over (groups).
    pub(super) fn parse(bytes: &'a [u8]) -> Option<Self> {
        let mut fields = Vec::new();
        let mut at = 0_usize;
        while at < bytes.len() {
            let key = read_varint(bytes, &mut at)?;
            let number = u32::try_from(key >> 3).ok().filter(|number| *number > 0)?;
            let value = match key & 7 {
                0 => Value::Varint(read_varint(bytes, &mut at)?),
                1 => {
                    let raw = bytes.get(at..at.checked_add(8)?)?;
                    at += 8;
                    Value::Fixed(u64::from_le_bytes(raw.try_into().ok()?))
                }
                2 => {
                    let len = usize::try_from(read_varint(bytes, &mut at)?).ok()?;
                    let raw = bytes.get(at..at.checked_add(len)?)?;
                    at += len;
                    Value::Bytes(raw)
                }
                5 => {
                    let raw = bytes.get(at..at.checked_add(4)?)?;
                    at += 4;
                    Value::Fixed(u64::from(u32::from_le_bytes(raw.try_into().ok()?)))
                }
                _ => return None,
            };
            fields.push((number, value));
        }
        Some(Self { fields })
    }

    fn first(&self, number: u32) -> Option<Value<'a>> {
        self.fields
            .iter()
            .find(|(field, _)| *field == number)
            .map(|(_, value)| *value)
    }

    /// A varint field; zero when it is absent (the proto3 default) or of
    /// another type.
    pub(super) fn varint(&self, number: u32) -> u64 {
        match self.first(number) {
            Some(Value::Varint(value)) => value,
            _ => 0,
        }
    }

    /// Whether the field is present at all.
    #[cfg(test)]
    pub(super) fn has(&self, number: u32) -> bool {
        self.first(number).is_some()
    }

    /// A length-delimited field's bytes.
    pub(super) fn bytes(&self, number: u32) -> Option<&'a [u8]> {
        match self.first(number) {
            Some(Value::Bytes(bytes)) => Some(bytes),
            _ => None,
        }
    }

    /// A string field; `None` unless it is valid UTF-8.
    pub(super) fn string(&self, number: u32) -> Option<&'a str> {
        std::str::from_utf8(self.bytes(number)?).ok()
    }

    /// A nested message; `None` when absent or not a message.
    pub(super) fn message(&self, number: u32) -> Option<Message<'a>> {
        Message::parse(self.bytes(number)?)
    }
}

/// Test helpers that build the wire format.
#[cfg(test)]
pub(super) mod encode {
    pub(crate) fn varint(mut value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return out;
            }
            out.push(byte | 0x80);
        }
    }

    pub(crate) fn varint_field(number: u32, value: u64) -> Vec<u8> {
        let mut out = varint(u64::from(number) << 3);
        out.extend(varint(value));
        out
    }

    pub(crate) fn bytes_field(number: u32, bytes: &[u8]) -> Vec<u8> {
        let mut out = varint((u64::from(number) << 3) | 2);
        out.extend(varint(bytes.len() as u64));
        out.extend_from_slice(bytes);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::encode::*;
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&text[at..at + 2], 16).expect("hex"))
            .collect()
    }

    #[test]
    fn varints_and_nested_fields_are_read_by_number() {
        let inner = [varint_field(1, 1417), varint_field(2, 1200)].concat();
        let blob = [
            bytes_field(1, &inner),
            bytes_field(4, b"id"),
            varint_field(9, 300),
        ]
        .concat();
        let message = Message::parse(&blob).expect("parse");
        assert_eq!(message.varint(9), 300);
        assert_eq!(message.varint(7), 0, "absent is zero");
        assert_eq!(message.string(4), Some("id"));
        let inner = message.message(1).expect("nested");
        assert_eq!(inner.varint(1), 1417);
        assert_eq!(inner.varint(2), 1200);
        assert!(inner.has(2) && !inner.has(3));
    }

    #[test]
    fn unknown_fields_are_stepped_over_and_garbage_is_refused() {
        // Fixed32, fixed64, and an unknown field around the one we want.
        let blob = [
            vec![0x25, 1, 0, 0, 0],
            vec![0x31, 1, 0, 0, 0, 0, 0, 0, 0],
            bytes_field(6, b"ignored"),
            varint_field(2, 7),
        ]
        .concat();
        assert_eq!(Message::parse(&blob).expect("parse").varint(2), 7);
        // Cut off in a length, in a varint, and a group (wire type 3).
        assert!(Message::parse(&[0x0a, 0x05, 1]).is_none());
        assert!(Message::parse(&[0x08, 0x80]).is_none());
        assert!(Message::parse(&[0x0b]).is_none());
        // Field number zero is not valid.
        assert!(Message::parse(&[0x00, 0x01]).is_none());
        assert!(Message::parse(&[]).is_some(), "an empty message is valid");
        // A varint of eleven bytes does not fit in 64 bits.
        assert!(Message::parse(&[
            0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01
        ])
        .is_none());
    }

    #[test]
    fn the_recorded_synthetic_blob_decodes_to_its_documented_values() {
        // A gen_metadata row with made-up values and the real structure.
        let blob = hex(concat!(
            "0a860118890b225b08890b10b00918de0228c03e30183a266d73672d3030303030303030",
            "303030303030303030303030303030303030303030303030303042100a0973657373696f",
            "6e4944120331323348ac0250325a0c726573702d69642d303030315a0808021080cab5ee",
            "01620708071080e59a779a011067656d696e692d342d6172676f6e2d6122243030303030",
            "3030302d303030302d303030302d303030302d303030303030303030303030"
        ));
        let row = Message::parse(&blob).expect("row");
        let usage = row
            .message(1)
            .and_then(|chat| chat.message(4))
            .expect("usage");
        assert_eq!(usage.varint(1), 1417, "model");
        assert_eq!(usage.varint(2), 1200, "uncached input");
        assert_eq!(usage.varint(3), 350, "output");
        assert_eq!(usage.varint(5), 8000, "cache read");
        assert_eq!(usage.varint(6), 24, "provider");
        assert_eq!(usage.varint(9), 300, "thinking");
        assert_eq!(usage.varint(10), 50, "response");
        assert_eq!(usage.string(11), Some("resp-id-0001"));
        let chat = row.message(1).expect("chat");
        let first_token = chat.message(11).expect("first token");
        assert_eq!(
            (first_token.varint(1), first_token.varint(2)),
            (2, 500_000_000)
        );
        assert_eq!(chat.string(19), Some("gemini-4-argon-a"));
    }
}
