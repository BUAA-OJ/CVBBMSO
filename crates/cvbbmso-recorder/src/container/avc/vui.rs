//! Chroma-location signalling in an H.264 sequence parameter set.
//!
//! `nclx` has no chroma-location field. Preserve the SPS and existing VUI
//! (including timing and HRD), replacing only its chroma-location description.

use super::ContainerError;

/// `chroma_loc_info_present_flag`, followed by two ue(v) values of 1 (center).
const CENTERED: [bool; 7] = [true, false, true, false, false, true, false];

pub(super) fn centered_chroma(sequence: &[u8]) -> Result<Vec<u8>, ContainerError> {
    if sequence.len() < 5 || sequence.len() > usize::from(u16::MAX) || sequence[0] & 0x9F != 7 {
        return Err(ContainerError::InvalidParameterSet);
    }
    let rbsp = unescape(&sequence[1..])?;
    let mut bits = Bits::new(&rbsp)?;
    skip_sequence(&mut bits)?;
    let vui_flag = bits.position;
    if bits.read(1)? == 0 {
        let mut vui = vec![true, false, false, false];
        vui.extend(CENTERED);
        // No timing, either HRD, pic_struct or bitstream restriction. These
        // fields were absent in the original SPS as well.
        vui.extend([false; 5]);
        bits.data.splice(vui_flag..=vui_flag, vui);
    } else {
        skip_vui_prefix(&mut bits)?;
        let start = bits.position;
        if bits.read(1)? != 0 {
            bits.ue(5)?;
            bits.ue(5)?;
        }
        bits.data.splice(start..bits.position, CENTERED);
    }
    let mut output = vec![sequence[0]];
    let mut zeros = 0;
    for chunk in bits.data.chunks(8) {
        let mut byte = 0;
        for (index, bit) in chunk.iter().enumerate() {
            byte |= u8::from(*bit) << (7 - index);
        }
        if zeros == 2 && byte <= 3 {
            output.push(3);
            zeros = 0;
        }
        output.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    if output.len() > usize::from(u16::MAX) {
        return Err(ContainerError::InvalidParameterSet);
    }
    Ok(output)
}

/// Removes emulation-prevention bytes, rejecting truncated/invalid escapes.
fn unescape(bytes: &[u8]) -> Result<Vec<u8>, ContainerError> {
    let mut result = Vec::with_capacity(bytes.len());
    let mut zeros = 0;
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        if zeros == 2 {
            if byte == 3 {
                if bytes.get(index + 1).is_none_or(|next| *next > 3) {
                    return Err(ContainerError::InvalidParameterSet);
                }
                zeros = 0;
                index += 1;
                continue;
            }
            if byte < 3 {
                return Err(ContainerError::InvalidParameterSet);
            }
        }
        result.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        index += 1;
    }
    Ok(result)
}

/// A bounded SPS bit cursor. The final stop bit is retained but never parsed
/// as syntax; byte-alignment padding is rebuilt after inserting VUI fields.
struct Bits {
    data: Vec<bool>,
    position: usize,
}

impl Bits {
    fn new(bytes: &[u8]) -> Result<Self, ContainerError> {
        if bytes.last().is_none_or(|byte| *byte == 0) {
            return Err(ContainerError::InvalidParameterSet);
        }
        let mut data: Vec<_> = bytes
            .iter()
            .flat_map(|byte| (0..8).rev().map(move |shift| byte & (1 << shift) != 0))
            .collect();
        while data.last() == Some(&false) {
            data.pop();
        }
        Ok(Self { data, position: 0 })
    }

    fn skip(&mut self, count: usize) -> Result<(), ContainerError> {
        let end = self
            .position
            .checked_add(count)
            .ok_or(ContainerError::InvalidParameterSet)?;
        if end >= self.data.len() {
            return Err(ContainerError::InvalidParameterSet);
        }
        self.position = end;
        Ok(())
    }

    fn read(&mut self, count: usize) -> Result<u32, ContainerError> {
        let start = self.position;
        self.skip(count)?;
        Ok(self.data[start..self.position]
            .iter()
            .fold(0, |value, bit| (value << 1) | u32::from(*bit)))
    }

    fn ue(&mut self, maximum: u32) -> Result<u32, ContainerError> {
        let mut leading = 0;
        while self.read(1)? == 0 {
            leading += 1;
            if leading > 31 {
                return Err(ContainerError::InvalidParameterSet);
            }
        }
        let value = ((1_u32 << leading) - 1) + self.read(leading)?;
        if value > maximum {
            return Err(ContainerError::InvalidParameterSet);
        }
        Ok(value)
    }

    fn se(&mut self) -> Result<i64, ContainerError> {
        let code = self.ue(u32::MAX)?;
        let magnitude = i64::from(code.div_ceil(2));
        Ok(if code.is_multiple_of(2) {
            -magnitude
        } else {
            magnitude
        })
    }
}

/// Locates the VUI flag without changing any decoding parameters.
fn skip_sequence(bits: &mut Bits) -> Result<(), ContainerError> {
    let profile = bits.read(8)?;
    bits.skip(16)?; // constraint flags and level
    bits.ue(31)?; // SPS id
    match profile {
        66 | 77 | 88 => {}
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135 => {
            let chroma = bits.ue(3)?;
            if chroma != 1 {
                // The recorder produces 4:2:0. Other subsampling formats do
                // not share this chroma-location contract.
                return Err(ContainerError::Unsupported);
            }
            bits.ue(6)?; // luma bit depth minus 8
            bits.ue(6)?; // chroma bit depth minus 8
            bits.skip(1)?; // transform bypass
            if bits.read(1)? != 0 {
                for index in 0..8 {
                    if bits.read(1)? != 0 {
                        skip_scaling_list(bits, if index < 6 { 16 } else { 64 })?;
                    }
                }
            }
        }
        _ => return Err(ContainerError::Unsupported),
    }
    bits.ue(12)?; // log2_max_frame_num_minus4
    match bits.ue(2)? {
        0 => {
            bits.ue(12)?;
        }
        1 => {
            bits.skip(1)?;
            bits.se()?;
            bits.se()?;
            for _ in 0..bits.ue(255)? {
                bits.se()?;
            }
        }
        _ => {}
    }
    bits.ue(u32::MAX)?; // reference frames
    bits.skip(1)?; // gaps allowed
    bits.ue(u32::MAX)?; // width in macroblocks minus 1
    bits.ue(u32::MAX)?; // height in map units minus 1
    if bits.read(1)? == 0 {
        bits.skip(1)?;
    } // adaptive field coding
    bits.skip(1)?; // direct_8x8_inference
    if bits.read(1)? != 0 {
        for _ in 0..4 {
            bits.ue(u32::MAX)?;
        }
    }
    Ok(())
}

fn skip_scaling_list(bits: &mut Bits, size: usize) -> Result<(), ContainerError> {
    let mut scale = 8_i64;
    for _ in 0..size {
        let delta = bits.se()?;
        if !(-128..=127).contains(&delta) {
            return Err(ContainerError::InvalidParameterSet);
        }
        scale = (scale + delta + 256) % 256;
        if scale == 0 {
            break;
        }
    }
    Ok(())
}

/// Leaves aspect ratio, overscan and video-signal fields intact.
fn skip_vui_prefix(bits: &mut Bits) -> Result<(), ContainerError> {
    if bits.read(1)? != 0 && bits.read(8)? == 255 {
        bits.skip(32)?;
    }
    if bits.read(1)? != 0 {
        bits.skip(1)?;
    }
    if bits.read(1)? != 0 {
        bits.skip(4)?; // video_format and full_range
        if bits.read(1)? != 0 {
            bits.skip(24)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
