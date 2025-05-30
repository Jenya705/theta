pub mod decode;
pub mod encode;

#[repr(transparent)]
#[derive(Debug, PartialEq, Eq)]
pub struct VarInt<T>(pub T);

#[repr(transparent)]
#[derive(Debug)]
pub struct StrWithMaxLen<'a, const MAX_LEN: usize>(pub &'a str);

impl<'a, const MAX_LEN: usize> StrWithMaxLen<'a, MAX_LEN> {
    pub fn new(str: &'a str) -> Self {
        Self(str)
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Debug;

    use crate::proto::VarInt;

    use super::{decode::Decode, encode::Encode};

    fn assert_encode_bytes<T>(val: T, bytes: &[u8])
    where
        T: Encode,
    {
        let mut v = vec![];
        assert!(val.encode(&mut v).is_ok());
        assert_eq!(v, bytes);
    }

    fn assert_decode_bytes<'a, T>(mut bytes: &'a [u8], val: T)
    where
        T: Decode<'a>,
        T: PartialEq + Eq,
        T: Debug,
    {
        let decoded = T::decode(&mut bytes);
        assert!(decoded.is_ok());
        assert_eq!(val, decoded.unwrap());
    }

    macro_rules! assert_mutually_codable {
        ($($val: expr),*$(,)?) => {
            $(
                {
                    let val = $val;
                    let mut v = vec![];
                    assert!(val.encode(&mut v).is_ok());
                    let decoded = <_>::decode(&mut v.as_slice());
                    assert_eq!(Ok(val), decoded);
                }
            )*
        }
    }

    #[test]
    fn encode_num_test() {
        assert_encode_bytes(0u8, &[0]);
        assert_encode_bytes(64u8, &[64]);
        assert_encode_bytes(0x77_aa as i16, &[0xaa, 0x77]);
        assert_encode_bytes(0xaa_bb_cc_dd as u32, &[0xdd, 0xcc, 0xbb, 0xaa]);
    }

    #[test]
    fn decode_num_test() {
        assert_decode_bytes(&[0], 0u8);
        assert_decode_bytes(&[64], 64u8);
        assert_decode_bytes(&[0xaa, 0x77], 0x77_aa as i16);
        assert_decode_bytes(&[0xdd, 0xcc, 0xbb, 0xaa], 0xaa_bb_cc_dd as u32);
    }

    #[test]
    fn mutually_codable_num_test() {
        assert_mutually_codable!(
            0u8,
            1u8,
            9u8,
            10u8,
            585u16,
            8321i16,
            3295678438u32,
            843825i32,
            -8756i64,
            -321i128,
        );
    }

    #[test]
    fn var_int_test() {
        assert_mutually_codable!(
            VarInt::<u32>(u32::MAX),
            VarInt::<u32>(32919312),
            VarInt::<u32>(1),
            VarInt::<u64>(321930205),
            VarInt::<u64>(u64::MAX),
            VarInt::<i32>(i32::MAX),
            VarInt::<i32>(i32::MIN),
            VarInt::<i32>(-3213),
        );
    }
}
