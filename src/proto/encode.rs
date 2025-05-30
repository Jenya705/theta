use super::{StrWithMaxLen, VarInt};

pub trait Encoder {
    fn write(&mut self, bytes: &[u8]) -> Result<(), ()>;
}

impl Encoder for Vec<u8> {
    fn write(&mut self, bytes: &[u8]) -> Result<(), ()> {
        self.extend_from_slice(bytes);
        Ok(())
    }
}

pub trait Encode {
    fn encode(&self, encoder: &mut impl Encoder) -> Result<(), ()>;
}

macro_rules! impl_num {
    ($($ty: ty)*) => {
        $(
            impl Encode for $ty {
                fn encode(&self, encoder: &mut impl Encoder) -> Result<(), ()> {
                    encoder.write(&self.to_le_bytes())
                }
            }
        )*
    }
}

impl_num!(u8 i8 u16 i16 u32 i32 u64 i64 u128 i128 f32 f64);

macro_rules! impl_var_int {
    ($($ty: ty,$unsigned:ty)*) => {
        $(
            impl Encode for crate::proto::VarInt<$ty> {
                fn encode(&self, encoder: &mut impl Encoder) -> Result<(), ()> {
                    let mut this = self.0 as $unsigned;

                    loop {
                        let mut to_write = (this & 0b0111_1111) as u8;
                        this>>=7;
                        if this != 0 {
                            to_write |= 0b1000_0000;
                        }
                        to_write.encode(encoder)?;
                        if this == 0 {
                            break;
                        }
                    }

                    Ok(())
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

impl<'a, const MAX_LEN: usize> Encode for StrWithMaxLen<'a, MAX_LEN> {
    fn encode(&self, encoder: &mut impl Encoder) -> Result<(), ()> {
        if self.0.len() > MAX_LEN {
            return Err(());
        }
        VarInt(self.0.len() as u32).encode(encoder)?;
        encoder.write(self.0.as_bytes())
    }
}
