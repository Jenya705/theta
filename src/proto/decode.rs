use super::{StrWithMaxLen, VarInt};

pub trait Decoder<'a> {
    fn read(&mut self, len: usize) -> Option<&'a [u8]>;

    fn read_byte(&mut self) -> Option<u8> {
        self.read(1).map(|v| v[0])
    }
}

impl<'a> Decoder<'a> for &'a [u8] {
    fn read(&mut self, len: usize) -> Option<&'a [u8]> {
        let (to_ret, rest) = self.split_at_checked(len)?;
        *self = rest;
        Some(to_ret)
    }

    fn read_byte(&mut self) -> Option<u8> {
        let to_ret = self.get(0).cloned()?;
        *self = &self[1..];
        Some(to_ret)
    }
}

pub trait Decode<'a>: 'a + Sized {
    fn decode(decoder: &mut impl Decoder<'a>) -> Result<Self, ()>;
}

macro_rules! impl_num {
    ($($ty: ty)*) => {
        $(
            impl<'a> Decode<'a> for $ty {
                fn decode(decoder: &mut impl Decoder<'a>) -> Result<Self, ()> {
                    Ok(Self::from_le_bytes(decoder.read(std::mem::size_of::<Self>()).ok_or(())?.try_into().unwrap()))
                }
            }
        )*
    }
}

impl_num!(u8 i8 u16 i16 u32 i32 u64 i64 u128 i128 f32 f64);

macro_rules! impl_var_int {
    ($($ty: ty,$unsigned:ty)*) => {
        $(
            impl<'a> Decode<'a> for crate::proto::VarInt<$ty> {
                fn decode(decoder: &mut impl Decoder<'a>) -> Result<Self, ()> {
                    let mut this: $unsigned = 0;
                    let mut shift = 0;

                    loop {
                        let byte = decoder.read_byte().ok_or(())?;
                        this |= ((byte & 0b0111_1111) as $unsigned) << shift;
                        if byte & 0b1000_0000 == 0 {
                            break;
                        }
                        shift+=7;
                    }

                    Ok(Self(this as $ty))
                }
            }
        )*
    }
}

impl_var_int!(
    u16, u16
    i16, u16
    u32, u32
    i32, u32
    u64, u64 
    i64, u64 
    u128, u128
    i128, u128
);

impl<'a, const MAX_LEN: usize> Decode<'a> for StrWithMaxLen<'a, MAX_LEN> {
    fn decode(decoder: &mut impl Decoder<'a>) -> Result<Self, ()> {
        let len = VarInt::<u32>::decode(decoder)?;
        if len.0 > MAX_LEN as u32 {
            return Err(());
        }
        let bytes = decoder.read(len.0 as usize).ok_or(())?;
        str::from_utf8(bytes).map_err(|_| ()).map(|v| Self(v))
    }
}
