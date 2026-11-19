//! AVC decoder configuration and NAL unit framing.

use crate::container::ContainerError;

/// Bytes preceding the parameter sets in an `AVCDecoderConfigurationRecord`.
const RECORD_HEADER: usize = 6;
/// Byte holding the NAL length size in the configuration record.
const LENGTH_SIZE_INDEX: usize = 4;
/// NAL length size used by this crate, in bytes.
const NAL_LENGTH_SIZE: u8 = 4;

/// H.264 parameter sets required to decode the recorded stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AvcConfig {
    sequence: Vec<u8>,
    picture: Vec<u8>,
}

impl AvcConfig {
    /// Builds a configuration from one sequence and one picture parameter set.
    ///
    /// # Errors
    ///
    /// Returns [`ContainerError::InvalidParameterSet`] when the sequence
    /// parameter set is too short to carry the profile bytes or when a
    /// parameter set does not fit the configuration record.
    pub(crate) fn new(sequence: Vec<u8>, picture: Vec<u8>) -> Result<Self, ContainerError> {
        let fits = u16::try_from(sequence.len()).is_ok() && u16::try_from(picture.len()).is_ok();
        if sequence.len() < 4 || picture.is_empty() || !fits {
            return Err(ContainerError::InvalidParameterSet);
        }
        Ok(Self { sequence, picture })
    }

    /// Renders the `AVCDecoderConfigurationRecord` stored as Matroska
    /// `CodecPrivate`.
    ///
    /// # Errors
    ///
    /// Returns [`ContainerError::InvalidParameterSet`] when a parameter set is
    /// too large for the record's 16-bit length field.
    pub(crate) fn record(&self) -> Result<Vec<u8>, ContainerError> {
        let sequence_length =
            u16::try_from(self.sequence.len()).map_err(|_| ContainerError::InvalidParameterSet)?;
        let picture_length =
            u16::try_from(self.picture.len()).map_err(|_| ContainerError::InvalidParameterSet)?;
        let mut record =
            Vec::with_capacity(RECORD_HEADER + self.sequence.len() + self.picture.len() + 8);
        record.push(1);
        record.extend_from_slice(&self.sequence[1..4]);
        // Six reserved bits followed by `lengthSizeMinusOne`.
        record.push(0xFC | (NAL_LENGTH_SIZE - 1));
        // Three reserved bits followed by the sequence parameter set count.
        record.push(0xE0 | 1);
        record.extend_from_slice(&sequence_length.to_be_bytes());
        record.extend_from_slice(&self.sequence);
        record.push(1);
        record.extend_from_slice(&picture_length.to_be_bytes());
        record.extend_from_slice(&self.picture);
        Ok(record)
    }

    /// Parses an `AVCDecoderConfigurationRecord`.
    ///
    /// # Errors
    ///
    /// Returns [`ContainerError::InvalidParameterSet`] when the record is
    /// truncated or carries no parameter set, and
    /// [`ContainerError::Unsupported`] when it does not use four-byte NAL
    /// lengths.
    pub(crate) fn parse(record: &[u8]) -> Result<Self, ContainerError> {
        if record.len() <= RECORD_HEADER || record[0] != 1 {
            return Err(ContainerError::InvalidParameterSet);
        }
        if record[LENGTH_SIZE_INDEX] & 0x03 != NAL_LENGTH_SIZE - 1 {
            return Err(ContainerError::Unsupported);
        }

        let mut offset = RECORD_HEADER;
        let sequence_count = record[RECORD_HEADER - 1] & 0x1F;
        if sequence_count != 1 {
            // The writer emits exactly one parameter set per kind, and this crate
            // does not represent multi-set configurations faithfully.
            return Err(if sequence_count == 0 {
                ContainerError::InvalidParameterSet
            } else {
                ContainerError::Unsupported
            });
        }
        let sequence = take_parameter_set(record, &mut offset)?;

        let picture_count = *record
            .get(offset)
            .ok_or(ContainerError::InvalidParameterSet)?;
        offset += 1;
        if picture_count != 1 {
            return Err(if picture_count == 0 {
                ContainerError::InvalidParameterSet
            } else {
                ContainerError::Unsupported
            });
        }
        let picture = take_parameter_set(record, &mut offset)?;

        Self::new(sequence, picture)
    }
}

/// Reads one length-prefixed parameter set and advances `offset`.
fn take_parameter_set(record: &[u8], offset: &mut usize) -> Result<Vec<u8>, ContainerError> {
    let length = record
        .get(*offset..*offset + 2)
        .map(|bytes| usize::from(u16::from_be_bytes([bytes[0], bytes[1]])))
        .ok_or(ContainerError::InvalidParameterSet)?;
    *offset += 2;
    let parameter_set = record
        .get(*offset..*offset + length)
        .ok_or(ContainerError::InvalidParameterSet)?;
    *offset += length;
    Ok(parameter_set.to_vec())
}

/// Splits an Annex B byte stream into NAL units, dropping start codes and
/// trailing padding.
pub(crate) fn annex_b_nals(stream: &[u8]) -> Vec<&[u8]> {
    let mut units = Vec::new();
    let mut start = None;
    let mut index = 0;
    while index < stream.len() {
        let rest = &stream[index..];
        let code = if rest.starts_with(&[0, 0, 0, 1]) {
            4
        } else if rest.starts_with(&[0, 0, 1]) {
            3
        } else {
            index += 1;
            continue;
        };
        if let Some(previous) = start {
            push_unit(&mut units, &stream[previous..index]);
        }
        index += code;
        start = Some(index);
    }
    if let Some(previous) = start {
        push_unit(&mut units, &stream[previous..]);
    }
    units
}

/// Frames NAL units with the four-byte length prefix used inside containers.
///
/// # Errors
///
/// Returns [`ContainerError::Unsupported`] when a NAL unit does not fit the
/// four-byte length prefix.
pub(crate) fn length_prefixed(units: &[&[u8]]) -> Result<Vec<u8>, ContainerError> {
    let mut framed = Vec::with_capacity(units.iter().map(|unit| unit.len() + 4).sum());
    for unit in units {
        let length = u32::try_from(unit.len()).map_err(|_| ContainerError::Unsupported)?;
        framed.extend_from_slice(&length.to_be_bytes());
        framed.extend_from_slice(unit);
    }
    Ok(framed)
}

/// Returns the NAL unit type encoded in the first byte of a NAL unit.
pub(crate) fn nal_type(unit: &[u8]) -> u8 {
    unit.first().map_or(0, |byte| byte & 0x1F)
}

fn push_unit<'a>(units: &mut Vec<&'a [u8]>, unit: &'a [u8]) {
    let trimmed = trim_trailing_zeros(unit);
    if !trimmed.is_empty() {
        units.push(trimmed);
    }
}

fn trim_trailing_zeros(mut unit: &[u8]) -> &[u8] {
    while let Some((last, rest)) = unit.split_last() {
        if *last != 0 {
            break;
        }
        unit = rest;
    }
    unit
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEQUENCE: [u8; 6] = [0x67, 0x42, 0xC0, 0x1E, 0xD9, 0x01];
    const PICTURE: [u8; 4] = [0x68, 0xCE, 0x38, 0x80];

    fn config() -> AvcConfig {
        AvcConfig::new(SEQUENCE.to_vec(), PICTURE.to_vec()).expect("configuration")
    }

    #[test]
    fn record_carries_the_parameter_sets() {
        let record = config().record().expect("record");

        assert_eq!(record[0], 1);
        assert_eq!(&record[1..4], &SEQUENCE[1..4]);
        assert_eq!(record[4], 0xFF);
        assert_eq!(record[5], 0xE1);
        assert_eq!(&record[6..8], &[0, 6]);
        assert_eq!(&record[8..14], &SEQUENCE);
        assert_eq!(record[14], 1);
        assert_eq!(&record[15..17], &[0, 4]);
        assert_eq!(&record[17..], &PICTURE);
    }

    #[test]
    fn record_round_trips() {
        let parsed = AvcConfig::parse(&config().record().expect("record")).expect("parsed");
        assert_eq!(parsed, config());
    }

    #[test]
    fn parse_rejects_a_truncated_record() {
        assert!(AvcConfig::parse(&[1, 0x42]).is_err());
        assert!(AvcConfig::parse(&[]).is_err());
    }

    #[test]
    fn new_rejects_a_short_sequence_parameter_set() {
        assert!(AvcConfig::new(vec![0x67, 0x42], PICTURE.to_vec()).is_err());
        assert!(AvcConfig::new(SEQUENCE.to_vec(), Vec::new()).is_err());
    }

    #[test]
    fn annex_b_splits_both_start_code_lengths() {
        let mut stream = vec![0, 0, 0, 1];
        stream.extend_from_slice(&SEQUENCE);
        stream.extend_from_slice(&[0, 0, 1]);
        stream.extend_from_slice(&PICTURE);
        stream.extend_from_slice(&[0, 0]);

        let units = annex_b_nals(&stream);

        assert_eq!(units.len(), 2);
        assert_eq!(units[0], SEQUENCE);
        assert_eq!(units[1], PICTURE);
        assert_eq!(nal_type(units[0]), 7);
        assert_eq!(nal_type(units[1]), 8);
    }

    #[test]
    fn annex_b_strips_trailing_zero_padding() {
        let mut stream = vec![0, 0, 1];
        stream.extend_from_slice(&SEQUENCE);
        stream.extend_from_slice(&[0, 0]);

        let units = annex_b_nals(&stream);

        assert_eq!(units, vec![SEQUENCE.as_slice()]);
    }

    #[test]
    fn length_prefixing_is_four_bytes() {
        let framed = length_prefixed(&[&SEQUENCE, &PICTURE]).expect("framed");

        assert_eq!(&framed[0..4], &[0, 0, 0, 6]);
        assert_eq!(&framed[4..10], &SEQUENCE);
        assert_eq!(&framed[10..14], &[0, 0, 0, 4]);
        assert_eq!(&framed[14..], &PICTURE);
    }
}
