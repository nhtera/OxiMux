//! Annex-B H.264 (NAL units separated by `00 00 01` / `00 00 00 01` start
//! codes) into the pieces a VideoToolbox session takes: the SPS and PPS for its
//! format description, and each picture as AVCC (every NAL unit prefixed by
//! its 4-byte big-endian length).

pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_IDR: u8 = 5;

/// The NAL units in `data`, without their start codes.
pub fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut units = Vec::with_capacity(starts.len());
    for (n, &start) in starts.iter().enumerate() {
        let mut end = starts.get(n + 1).map_or(data.len(), |next| next - 3);
        // A 4-byte start code leaves its leading zero on the previous unit.
        while end > start && data[end - 1] == 0 && starts.get(n + 1).is_some() {
            end -= 1;
        }
        if end > start {
            units.push(&data[start..end]);
        }
    }
    units
}

/// `nal_unit_type`: the low five bits of the first byte.
pub fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |b| b & 0x1f)
}

/// The SPS and PPS of a stream, for its format description.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParameterSets {
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
}

/// The parameter sets in a config packet (the first SPS and PPS), if both are
/// there.
pub fn parameter_sets(config: &[u8]) -> Option<ParameterSets> {
    let units = nal_units(config);
    let sps = units.iter().find(|n| nal_type(n) == NAL_SPS)?;
    let pps = units.iter().find(|n| nal_type(n) == NAL_PPS)?;
    Some(ParameterSets { sps: sps.to_vec(), pps: pps.to_vec() })
}

/// A picture as an AVCC sample. Parameter sets are left out: they belong in
/// the format description, and repeating them in a sample confuses nothing
/// but helps nothing either.
pub fn to_avcc(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 16);
    for nal in nal_units(data).into_iter().filter(|n| !matches!(nal_type(n), NAL_SPS | NAL_PPS)) {
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config packet (SPS + PPS, 4-byte start codes) followed by a
    /// 3-byte-start-coded IDR slice, as MediaCodec can emit them.
    fn stream() -> Vec<u8> {
        let mut b = vec![0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1f];
        b.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80]);
        b.extend_from_slice(&[0, 0, 1, 0x65, 0x88, 0x84, 0x00]);
        b
    }

    #[test]
    fn units_split_on_both_start_codes() {
        let s = stream();
        let units = nal_units(&s);
        assert_eq!(units, [&[0x67, 0x42, 0x00, 0x1f][..], &[0x68, 0xce, 0x3c, 0x80][..], &[0x65, 0x88, 0x84, 0x00][..]]);
        assert_eq!(units.iter().map(|n| nal_type(n)).collect::<Vec<_>>(), [NAL_SPS, NAL_PPS, NAL_IDR]);
        assert!(nal_units(&[]).is_empty());
        assert!(nal_units(&[1, 2, 3]).is_empty(), "no start code, no units");
    }

    #[test]
    fn parameter_sets_come_from_the_config_packet() {
        let sets = parameter_sets(&stream()).unwrap();
        assert_eq!(sets.sps, [0x67, 0x42, 0x00, 0x1f]);
        assert_eq!(sets.pps, [0x68, 0xce, 0x3c, 0x80]);
        assert_eq!(parameter_sets(&[0, 0, 1, 0x65, 1]), None);
    }

    #[test]
    fn a_picture_becomes_length_prefixed_without_parameter_sets() {
        assert_eq!(to_avcc(&stream()), [0, 0, 0, 4, 0x65, 0x88, 0x84, 0x00]);
    }
}
