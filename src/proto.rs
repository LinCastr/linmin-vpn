//! Radmin GUI <-> service wire protocol: frame boundaries and the TLV body.
//!
//! One message is a frame: a little-endian `u32` length followed by that many
//! body bytes. The body is a flat list of attributes; a container attribute
//! holds a nested list. Each attribute header is 8 bytes:
//!
//! | bytes | field        | encoding        |
//! |-------|--------------|-----------------|
//! | 4     | value length | big-endian u32  |
//! | 1     | type         | see `ty`        |
//! | 1     | flags        | string selector |
//! | 2     | tag          | big-endian u16  |
//!
//! See `docs/PROTOCOL.md` for the full reverse-engineering notes.

#![allow(dead_code)]

// Attribute type bytes.
pub mod ty {
    pub const CONTAINER: u8 = 0x00;
    pub const U32: u8 = 0x01;
    pub const U64: u8 = 0x02;
    pub const STRING: u8 = 0x03; // UTF-16BE, often terminated by 0x0001
    pub const BYTES: u8 = 0x08;
    pub const IPV6: u8 = 0x09; // 16 raw bytes
    pub const BOOL: u8 = 0x0b; // big-endian u32, 0/1
    pub const GUID: u8 = 0x0d; // 16 raw bytes
}

/// A decoded attribute value. Scalars keep their wire width; `String` is already
/// decoded from UTF-16BE with the trailing `0x0001` marker removed.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Container(Vec<Attr>),
    U32(u32),
    U64(u64),
    Bool(bool),
    Str(String),
    Bytes(Vec<u8>),
    /// 16-byte IPv6 address, as seen on the wire.
    Ipv6([u8; 16]),
    /// 16-byte network GUID / 128-bit id, as seen on the wire.
    Guid([u8; 16]),
}

/// One tag/value pair. `flags` is kept because for strings it selects the
/// encoding; it is 0 for everything else observed so far.
#[derive(Debug, Clone, PartialEq)]
pub struct Attr {
    pub tag: u16,
    pub flags: u8,
    pub value: Value,
}

impl Attr {
    pub fn new(tag: u16, value: Value) -> Self {
        Attr { tag, flags: 0, value }
    }

    pub fn u32(tag: u16, v: u32) -> Self {
        Attr::new(tag, Value::U32(v))
    }

    pub fn bool(tag: u16, v: bool) -> Self {
        Attr::new(tag, Value::Bool(v))
    }

    pub fn str(tag: u16, v: &str) -> Self {
        Attr::new(tag, Value::Str(v.to_string()))
    }

    pub fn guid(tag: u16, v: [u8; 16]) -> Self {
        Attr::new(tag, Value::Guid(v))
    }

    pub fn bytes(tag: u16, v: Vec<u8>) -> Self {
        Attr::new(tag, Value::Bytes(v))
    }

    pub fn container(tag: u16, attrs: Vec<Attr>) -> Self {
        Attr::new(tag, Value::Container(attrs))
    }

    /// Finds the first direct child attribute with this tag (containers only).
    pub fn get(&self, tag: u16) -> Option<&Attr> {
        match &self.value {
            Value::Container(a) => a.iter().find(|x| x.tag == tag),
            _ => None,
        }
    }
}

/// Error from decoding a frame body or a frame reader.
#[derive(Debug, PartialEq)]
pub enum Error {
    /// Fewer bytes than an attribute header or its declared value needs.
    Truncated,
    /// A length field claims more bytes than the message can hold.
    BadLength,
    /// A fixed-width type carried the wrong value length.
    BadWidth(u8),
    /// Odd byte count for a UTF-16 string.
    BadString,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Truncated => write!(f, "truncated attribute"),
            Error::BadLength => write!(f, "value length exceeds buffer"),
            Error::BadWidth(t) => write!(f, "wrong width for type 0x{t:02x}"),
            Error::BadString => write!(f, "odd-length UTF-16 string"),
        }
    }
}

impl std::error::Error for Error {}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

/// Decodes a message body (no frame length prefix) into a flat attribute list.
pub fn decode_body(buf: &[u8]) -> Result<Vec<Attr>, Error> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        if i + 8 > buf.len() {
            return Err(Error::Truncated);
        }
        let vlen = u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) as usize;
        let ty = buf[i + 4];
        let flags = buf[i + 5];
        let tag = u16::from_be_bytes([buf[i + 6], buf[i + 7]]);
        i += 8;
        if i + vlen > buf.len() {
            return Err(Error::BadLength);
        }
        let val = &buf[i..i + vlen];
        i += vlen;
        let value = decode_value(ty, val)?;
        out.push(Attr { tag, flags, value });
    }
    Ok(out)
}

fn decode_value(ty: u8, val: &[u8]) -> Result<Value, Error> {
    Ok(match ty {
        ty::CONTAINER => Value::Container(decode_body(val)?),
        ty::U32 => Value::U32(read_u32(val)?),
        ty::U64 => {
            if val.len() != 8 {
                return Err(Error::BadWidth(ty));
            }
            Value::U64(u64::from_be_bytes(val.try_into().unwrap()))
        }
        ty::BOOL => Value::Bool(read_u32(val)? != 0),
        ty::STRING => Value::Str(decode_utf16be(val)?),
        ty::IPV6 => Value::Ipv6(read_16(val)?),
        ty::GUID => Value::Guid(read_16(val)?),
        // Unknown/raw types are preserved verbatim so nothing is lost.
        _ => Value::Bytes(val.to_vec()),
    })
}

fn read_u32(val: &[u8]) -> Result<u32, Error> {
    if val.len() != 4 {
        return Err(Error::BadWidth(ty::U32));
    }
    Ok(u32::from_be_bytes(val.try_into().unwrap()))
}

fn read_16(val: &[u8]) -> Result<[u8; 16], Error> {
    val.try_into().map_err(|_| Error::BadWidth(ty::GUID))
}

/// Decodes UTF-16BE, dropping a trailing `0x0001` terminator if present.
/// Unpaired surrogates become U+FFFD rather than failing the whole frame.
fn decode_utf16be(val: &[u8]) -> Result<String, Error> {
    if val.len() % 2 != 0 {
        return Err(Error::BadString);
    }
    let mut units: Vec<u16> = val
        .chunks_exact(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();
    if units.last() == Some(&0x0001) {
        units.pop();
    }
    Ok(String::from_utf16_lossy(&units))
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

/// Encodes a message body (attribute list) with no frame length prefix.
pub fn encode_body(attrs: &[Attr]) -> Vec<u8> {
    let mut out = Vec::new();
    for a in attrs {
        encode_attr(a, &mut out);
    }
    out
}

/// Encodes a body and wraps it in a frame: little-endian `u32` length + body.
pub fn encode_frame(attrs: &[Attr]) -> Vec<u8> {
    let body = encode_body(attrs);
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

fn encode_attr(a: &Attr, out: &mut Vec<u8>) {
    let (ty, val) = encode_value(&a.value);
    out.extend_from_slice(&(val.len() as u32).to_be_bytes());
    out.push(ty);
    out.push(a.flags);
    out.extend_from_slice(&a.tag.to_be_bytes());
    out.extend_from_slice(&val);
}

fn encode_value(v: &Value) -> (u8, Vec<u8>) {
    match v {
        Value::Container(a) => (ty::CONTAINER, encode_body(a)),
        Value::U32(n) => (ty::U32, n.to_be_bytes().to_vec()),
        Value::U64(n) => (ty::U64, n.to_be_bytes().to_vec()),
        Value::Bool(b) => (ty::BOOL, (*b as u32).to_be_bytes().to_vec()),
        Value::Str(s) => (ty::STRING, encode_utf16be(s)),
        Value::Bytes(b) => (ty::BYTES, b.clone()),
        Value::Ipv6(b) => (ty::IPV6, b.to_vec()),
        Value::Guid(b) => (ty::GUID, b.to_vec()),
    }
}

/// Encodes a string as UTF-16BE with the trailing `0x0001` marker the GUI sends.
fn encode_utf16be(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for u in s.encode_utf16() {
        out.extend_from_slice(&u.to_be_bytes());
    }
    out.extend_from_slice(&0x0001u16.to_be_bytes());
    out
}

// ---------------------------------------------------------------------------
// Stream de-framing
// ---------------------------------------------------------------------------

/// Reassembles frames from a byte stream. The pipe is read as bytes and a single
/// frame may span several reads, so bytes are buffered until a whole frame (its
/// little-endian `u32` length plus that many body bytes) is available.
#[derive(Default)]
pub struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    pub fn new() -> Self {
        FrameReader { buf: Vec::new() }
    }

    /// Appends freshly read bytes to the internal buffer.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Removes and returns the next complete frame body, or `None` if the
    /// buffer does not yet hold a full frame.
    pub fn next_frame(&mut self) -> Option<Vec<u8>> {
        if self.buf.len() < 4 {
            return None;
        }
        let len = u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        if self.buf.len() < 4 + len {
            return None;
        }
        let body = self.buf[4..4 + len].to_vec();
        self.buf.drain(..4 + len);
        Some(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_scalars() {
        let attrs = vec![
            Attr::u32(0x034a, 0xdead_beef),
            Attr::u32(0x0300, 4),
            Attr::bool(0x0311, true),
            Attr::new(0x0381, Value::U64(0x0102_0304_0506_0708)),
            Attr::guid(0x0309, [0x11; 16]),
        ];
        let body = encode_body(&attrs);
        assert_eq!(decode_body(&body).unwrap(), attrs);
    }

    #[test]
    fn roundtrip_container_and_string() {
        let attrs = vec![Attr::container(
            0x132b,
            vec![Attr::bool(0x0308, false), Attr::str(0x0306, "Volakia2")],
        )];
        let body = encode_body(&attrs);
        let got = decode_body(&body).unwrap();
        assert_eq!(got, attrs);
        // The string round-trips without the wire terminator leaking in.
        assert_eq!(got[0].get(0x0306).unwrap().value, Value::Str("Volakia2".into()));
    }

    #[test]
    fn string_is_utf16be_with_terminator() {
        let a = Attr::str(0x0306, "Ab");
        let mut buf = Vec::new();
        encode_attr(&a, &mut buf);
        // header(8) + "A"(2) + "b"(2) + term(2) = 14; value length = 6
        assert_eq!(&buf[0..4], &6u32.to_be_bytes());
        assert_eq!(buf[4], ty::STRING);
        assert_eq!(&buf[8..], &[0x00, 0x41, 0x00, 0x62, 0x00, 0x01]);
    }

    #[test]
    fn frame_prefix_is_le() {
        let f = encode_frame(&[Attr::u32(0x0300, 5)]);
        // body = header(8) + value(4) = 12
        assert_eq!(&f[0..4], &12u32.to_le_bytes());
        assert_eq!(f.len(), 16);
    }

    #[test]
    fn truncated_header_errors() {
        assert_eq!(decode_body(&[0, 0, 0]), Err(Error::Truncated));
    }

    #[test]
    fn bad_length_errors() {
        // Declares 4 value bytes but provides none.
        let buf = [0, 0, 0, 4, ty::U32, 0, 0x03, 0x00];
        assert_eq!(decode_body(&buf), Err(Error::BadLength));
    }

    #[test]
    fn framereader_splits_and_reassembles() {
        let f1 = encode_frame(&[Attr::u32(0x0300, 4)]);
        let f2 = encode_frame(&[Attr::u32(0x0300, 5)]);
        let mut stream = Vec::new();
        stream.extend_from_slice(&f1);
        stream.extend_from_slice(&f2);
        let mut r = FrameReader::new();
        // Feed one byte at a time to prove boundary reassembly.
        for b in &stream {
            r.feed(&[*b]);
        }
        let b1 = r.next_frame().unwrap();
        let b2 = r.next_frame().unwrap();
        assert_eq!(r.next_frame(), None);
        assert_eq!(decode_body(&b1).unwrap()[0].value, Value::U32(4));
        assert_eq!(decode_body(&b2).unwrap()[0].value, Value::U32(5));
    }
}
