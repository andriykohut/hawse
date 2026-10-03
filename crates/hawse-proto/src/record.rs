//! Records on a stream of the TCP transport. yamux ends a reset stream the way it ends a finished
//! one, so the finish is written into the stream: one that ends without it was aborted.

pub const DATA: u8 = 0x00;
pub const FINISH: u8 = 0x01;
pub const RESET: u8 = 0x02;

/// The most one data record carries.
pub const MAX_DATA: usize = 65_535;
/// The longest header: a reset's tag and its code.
pub const MAX_HEAD: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Head {
    /// That many payload bytes follow. Never zero.
    Data(u16),
    Finish,
    Reset(u32),
}

/// How many bytes a record's header takes, its tag included. `None` for a tag this build does not
/// know.
pub fn head_len(tag: u8) -> Option<usize> {
    match tag {
        DATA => Some(3),
        FINISH => Some(1),
        RESET => Some(5),
        _ => None,
    }
}

/// `head` is a whole header, `head_len` bytes of it. `None` when it is not one: the wrong length,
/// an unknown tag, or a data record of no bytes.
pub fn decode(head: &[u8]) -> Option<Head> {
    match *head {
        [DATA, lo, hi] => match u16::from_le_bytes([lo, hi]) {
            0 => None,
            len => Some(Head::Data(len)),
        },
        [FINISH] => Some(Head::Finish),
        [RESET, a, b, c, d] => Some(Head::Reset(u32::from_le_bytes([a, b, c, d]))),
        _ => None,
    }
}

/// The header of a data record. The caller keeps `len` above zero.
pub fn data(len: u16) -> [u8; 3] {
    let [lo, hi] = len.to_le_bytes();
    [DATA, lo, hi]
}

pub fn reset(code: u32) -> [u8; 5] {
    let [a, b, c, d] = code.to_le_bytes();
    [RESET, a, b, c, d]
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Against literals: these bytes are the wire format, so a constant pointed elsewhere has to
    /// fail here.
    #[test]
    fn every_record_keeps_its_wire_bytes() {
        assert_eq!(data(5), [0x00, 0x05, 0x00]);
        assert_eq!(data(0x1234), [0x00, 0x34, 0x12]);
        assert_eq!([FINISH], [0x01]);
        assert_eq!(reset(0x11), [0x02, 0x11, 0x00, 0x00, 0x00]);
        assert_eq!(reset(0x0102_0304), [0x02, 0x04, 0x03, 0x02, 0x01]);
    }

    #[test]
    fn a_header_is_as_long_as_its_tag_says() {
        assert_eq!(head_len(DATA), Some(3));
        assert_eq!(head_len(FINISH), Some(1));
        assert_eq!(head_len(RESET), Some(5));
        assert_eq!(head_len(0x03), None);
        assert_eq!(head_len(0xff), None);
    }

    #[test]
    fn a_finish_decodes() {
        assert_eq!(decode(&[FINISH]), Some(Head::Finish));
    }

    #[test]
    fn a_data_record_of_no_bytes_is_malformed() {
        assert_eq!(decode(&[DATA, 0, 0]), None);
    }

    #[test]
    fn a_header_of_the_wrong_length_is_malformed() {
        assert_eq!(decode(&[]), None);
        assert_eq!(decode(&[DATA, 5]), None);
        assert_eq!(decode(&[FINISH, 0]), None);
        assert_eq!(decode(&[RESET, 1, 2, 3]), None);
        assert_eq!(decode(&[0x7f]), None);
    }

    proptest! {
        #[test]
        fn data_headers_round_trip(len in 1u16..) {
            prop_assert_eq!(decode(&data(len)), Some(Head::Data(len)));
        }

        #[test]
        fn reset_headers_round_trip(code: u32) {
            prop_assert_eq!(decode(&reset(code)), Some(Head::Reset(code)));
        }
    }
}
