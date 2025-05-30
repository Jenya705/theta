use bevy::{log, platform_support::collections::HashMap};

use crate::proto::{
    VarInt,
    decode::{Decode, Decoder},
    encode::{Encode, Encoder},
};

#[derive(Default)]
pub(crate) struct PacketSegmentJoiner {
    map: HashMap<u32, PacketSegments>,
    capacity: usize,
}

pub(crate) struct PacketSegments {
    segments: Vec<Option<Vec<u8>>>,
    non_filled: usize,
}

pub(crate) struct PacketSegmentMetadata {
    pub segment_key: u32,
    pub segment: usize,
    pub segment_count: usize,
}

pub(crate) type PacketSegmentMetadataIntType = u16;

impl<'a> Decode<'a> for PacketSegmentMetadata {
    fn decode(decoder: &mut impl Decoder<'a>) -> Result<Self, ()> {
        Ok(Self {
            segment_key: VarInt::<u32>::decode(decoder)?.0,
            segment: VarInt::<PacketSegmentMetadataIntType>::decode(decoder)?.0 as usize,
            segment_count: VarInt::<PacketSegmentMetadataIntType>::decode(decoder)?.0 as usize + 1,
        })
    }
}

impl Encode for PacketSegmentMetadata {
    fn encode(&self, encoder: &mut impl Encoder) -> Result<(), ()> {
        debug_assert!(self.segment_count != 0);
        VarInt(self.segment_key).encode(encoder)?;
        VarInt::<PacketSegmentMetadataIntType>(self.segment.try_into().unwrap()).encode(encoder)?;
        VarInt::<PacketSegmentMetadataIntType>((self.segment_count - 1).try_into().unwrap())
            .encode(encoder)?;
        Ok(())
    }
}

#[derive(thiserror::Error, Debug)]
pub(crate) enum PacketSegmentJoinerError {
    #[error("Reached the maximum capacity")]
    CapacityReached,
    #[error("Segment was received twice or more")]
    SegmentRepeated,
    #[error("Segment metadata was malformed")]
    SegmentMetadataMalformed,
}

impl PacketSegmentJoiner {
    pub fn pop_first(&mut self) -> Option<()> {
        // TODO: try to find a way to find an "optimal" packet for removal
        let key = *self.map.keys().next()?;
        self.map.remove(&key).map(|_| ())
    }

    pub fn add(
        &mut self,
        metadata: PacketSegmentMetadata,
        payload: Vec<u8>,
        pop_first_on_capacity: bool,
    ) -> Result<Option<Vec<Vec<u8>>>, PacketSegmentJoinerError> {
        let segments = match self.map.get_mut(&metadata.segment_key) {
            Some(segments) => segments,
            None => {
                if self.capacity == self.map.len() {
                    if pop_first_on_capacity {
                        log::warn!(
                            "removing a random segment as there is no space for the new added one"
                        );
                        let _ = self.pop_first();
                    } else {
                        return Err(PacketSegmentJoinerError::CapacityReached);
                    }
                }

                if let Some(_) = self.map.insert(
                    metadata.segment_key,
                    PacketSegments {
                        segments: vec![None; metadata.segment_count],
                        non_filled: metadata.segment_count,
                    },
                ) {
                    // not really an error because we expect this kind of errors
                    // yet we have no mechanism of mitigating it in case of its happening
                    log::warn!(
                        "The new segment has overwritten an old one, because the keys are the same (key: {:?})",
                        metadata.segment_key
                    );
                }
                self.map.get_mut(&metadata.segment_key).unwrap()
            }
        };

        if metadata.segment_count != segments.segments.len() {
            return Err(PacketSegmentJoinerError::SegmentMetadataMalformed);
        }

        if segments.segments[metadata.segment].is_some() {
            return Err(PacketSegmentJoinerError::SegmentRepeated);
        }

        segments.segments[metadata.segment] = Some(payload);
        segments.non_filled -= 1;

        Ok(if segments.non_filled == 0 {
            let segments = self.map.remove(&metadata.segment_key).unwrap();
            Some(
                segments
                    .segments
                    .into_iter()
                    .map(Option::unwrap)
                    .collect::<Vec<_>>(),
            )
        } else {
            None
        })
    }
}
