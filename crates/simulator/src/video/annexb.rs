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

/// The parameter sets in an avcC record (ISO/IEC 14496-15 §5.2.4.1: the
/// first SPS and PPS), if it is well formed and uses 4-byte NAL lengths —
/// the only length VideoToolbox's encoder writes and our decoder reads.
pub fn from_avcc_record(record: &[u8]) -> Option<ParameterSets> {
    let (&version, rest) = record.split_first()?;
    if version != 1 || rest.len() < 5 || rest[3] & 0x03 != 3 {
        return None;
    }
    let mut at = &rest[5 - 1..]; // the SPS count byte
    let (sps, after) = first_set(at, at.first()? & 0x1f)?;
    at = after;
    let (pps, _) = first_set(at, *at.first()?)?;
    Some(ParameterSets { sps, pps })
}

/// The first of `count` u16-length-prefixed sets after the count byte, and
/// what follows all of them.
fn first_set(data: &[u8], count: u8) -> Option<(Vec<u8>, &[u8])> {
    let mut at = data.get(1..)?;
    let mut first = None;
    for _ in 0..count {
        let len = u16::from_be_bytes([*at.first()?, *at.get(1)?]) as usize;
        let set = at.get(2..2 + len)?;
        first.get_or_insert_with(|| set.to_vec());
        at = &at[2 + len..];
    }
    Some((first.filter(|s| !s.is_empty())?, at))
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
    fn parameter_sets_come_from_an_avcc_record() {
        // version, profile/compat/level, 0xFF (4-byte lengths), 1 SPS, 1 PPS.
        let record = [1, 0x64, 0x00, 0x1f, 0xff, 0xe1, 0, 4, 0x67, 0x64, 0x00, 0x1f, 1, 0, 3, 0x68, 0xee, 0x3c];
        let sets = from_avcc_record(&record).unwrap();
        assert_eq!(sets.sps, [0x67, 0x64, 0x00, 0x1f]);
        assert_eq!(sets.pps, [0x68, 0xee, 0x3c]);
        // Truncated anywhere, a wrong version, or 2-byte NAL lengths: refused.
        for cut in 0..record.len() {
            assert_eq!(from_avcc_record(&record[..cut]), None, "cut at {cut}");
        }
        let mut v2 = record;
        v2[0] = 2;
        assert_eq!(from_avcc_record(&v2), None);
        let mut short_lengths = record;
        short_lengths[4] = 0xfd;
        assert_eq!(from_avcc_record(&short_lengths), None);
        // The conformance helper's 4-byte stub has no sets.
        assert_eq!(from_avcc_record(&[0x01, 0x64, 0x00, 0x1f]), None);
    }

    #[test]
    fn a_picture_becomes_length_prefixed_without_parameter_sets() {
        assert_eq!(to_avcc(&stream()), [0, 0, 0, 4, 0x65, 0x88, 0x84, 0x00]);
    }
}
