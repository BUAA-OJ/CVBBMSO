//! Minimal EBML codec for the Matroska segment files this crate writes.

use crate::container::ContainerError;

/// Longest identifier this crate writes or accepts.
const MAX_IDENTIFIER_LEN: usize = 4;
/// Longest size descriptor this crate writes or accepts.
const MAX_SIZE_LEN: usize = 8;

/// Appends an element identifier in its canonical byte form.
pub(super) fn identifier(buffer: &mut Vec<u8>, id: u32) {
    let bytes = id.to_be_bytes();
    let first = bytes
        .iter()
        .position(|byte| *byte != 0)
        .unwrap_or(bytes.len() - 1);
    buffer.extend_from_slice(&bytes[first..]);
}

/// Appends an element size descriptor.
pub(super) fn size(buffer: &mut Vec<u8>, value: u64) {
    let mut length = 1;
    while length < MAX_SIZE_LEN && value >= (1_u64 << (7 * length)) - 1 {
        length += 1;
    }
    let marked = value | (1_u64 << (7 * length));
    for index in (0..length).rev() {
        buffer.push(((marked >> (8 * index)) & 0xFF) as u8);
    }
}

/// Appends a complete element with a known payload.
pub(super) fn element(buffer: &mut Vec<u8>, id: u32, payload: &[u8]) {
    identifier(buffer, id);
    size(buffer, payload.len() as u64);
    buffer.extend_from_slice(payload);
}

/// Appends an unsigned integer element, using the shortest encoding.
pub(super) fn unsigned(buffer: &mut Vec<u8>, id: u32, value: u64) {
    let bytes = value.to_be_bytes();
    let first = bytes
        .iter()
        .position(|byte| *byte != 0)
        .unwrap_or(bytes.len() - 1);
    element(buffer, id, &bytes[first..]);
}

/// Appends a UTF-8 string element.
pub(super) fn text(buffer: &mut Vec<u8>, id: u32, value: &str) {
    element(buffer, id, value.as_bytes());
}

/// Appends a 64-bit float element.
pub(super) fn float(buffer: &mut Vec<u8>, id: u32, value: f64) {
    element(buffer, id, &value.to_be_bytes());
}

/// Sequential reader over a byte slice of EBML elements.
pub(super) struct Parser<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> Parser<'a> {
    pub(super) const fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    pub(super) const fn is_empty(&self) -> bool {
        self.position >= self.data.len()
    }

    /// Reads the next element identifier and payload, or `None` at the end.
    pub(super) fn next_element(&mut self) -> Result<Option<(u32, &'a [u8])>, ContainerError> {
        if self.is_empty() {
            return Ok(None);
        }
        let id = self.identifier()?;
        let length = self.size()?;
        let payload = self.bytes(length)?;
        Ok(Some((id, payload)))
    }

    /// Reads an unsigned integer payload of at most eight bytes.
    pub(super) fn unsigned(payload: &[u8]) -> Result<u64, ContainerError> {
        if payload.len() > 8 {
            return Err(ContainerError::InvalidContainer);
        }
        let mut value = 0_u64;
        for byte in payload {
            value = (value << 8) | u64::from(*byte);
        }
        Ok(value)
    }

    fn identifier(&mut self) -> Result<u32, ContainerError> {
        let first = *self
            .data
            .get(self.position)
            .ok_or(ContainerError::InvalidContainer)?;
        let length = first.leading_zeros() as usize + 1;
        if length > MAX_IDENTIFIER_LEN {
            return Err(ContainerError::InvalidContainer);
        }
        let bytes = self.bytes(length)?;
        Ok(bytes
            .iter()
            .fold(0_u32, |value, byte| (value << 8) | u32::from(*byte)))
    }

    fn size(&mut self) -> Result<usize, ContainerError> {
        let first = *self
            .data
            .get(self.position)
            .ok_or(ContainerError::InvalidContainer)?;
        let length = first.leading_zeros() as usize + 1;
        if length > MAX_SIZE_LEN {
            return Err(ContainerError::InvalidContainer);
        }
        let bytes = self.bytes(length)?;
        let value_bits = u64::from(u32::from(u8::MAX) >> length);
        let mut value = u64::from(bytes[0]) & value_bits;
        for byte in &bytes[1..] {
            value = (value << 8) | u64::from(*byte);
        }
        // An all-ones value marks an element of unknown length; the files this
        // crate writes always carry explicit sizes.
        if value == (1_u64 << (7 * length)) - 1 {
            return Err(ContainerError::Unsupported);
        }
        usize::try_from(value).map_err(|_| ContainerError::InvalidContainer)
    }

    fn bytes(&mut self, length: usize) -> Result<&'a [u8], ContainerError> {
        let end = self
            .position
            .checked_add(length)
            .filter(|end| *end <= self.data.len())
            .ok_or(ContainerError::InvalidContainer)?;
        let slice = &self.data[self.position..end];
        self.position = end;
        Ok(slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_uses_minimal_bytes() {
        let mut buffer = Vec::new();
        identifier(&mut buffer, 0x1A45_DFA3);
        assert_eq!(buffer, [0x1A, 0x45, 0xDF, 0xA3]);

        buffer.clear();
        identifier(&mut buffer, 0xAE);
        assert_eq!(buffer, [0xAE]);
    }

    #[test]
    fn size_encoding_marks_the_length() {
        let mut buffer = Vec::new();
        size(&mut buffer, 5);
        assert_eq!(buffer, [0x85]);

        buffer.clear();
        size(&mut buffer, 200);
        assert_eq!(buffer, [0x40, 0xC8]);
    }

    #[test]
    fn round_trips_an_element() {
        let mut buffer = Vec::new();
        unsigned(&mut buffer, 0x002A_D7B1, 1_000_000);

        let mut parser = Parser::new(&buffer);
        let (id, payload) = parser.next_element().expect("element").expect("present");

        assert_eq!(id, 0x002A_D7B1);
        assert_eq!(Parser::unsigned(payload).expect("unsigned"), 1_000_000);
        assert!(parser.is_empty());
    }

    #[test]
    fn rejects_truncated_payload() {
        let buffer = [0x85];
        let mut parser = Parser::new(&buffer);
        assert!(parser.next_element().is_err());
    }
}
