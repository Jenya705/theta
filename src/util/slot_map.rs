use bevy::platform_support::collections::{HashMap, HashSet};

use crate::proto::{
    VarInt,
    decode::{Decode, Decoder},
    encode::{Encode, Encoder},
};

pub struct SlotMap<T> {
    values: Vec<Option<T>>,
    generations: Vec<u32>,
    free_slots: Vec<u32>,
}

impl<T> Default for SlotMap<T> {
    fn default() -> Self {
        Self {
            values: Default::default(),
            generations: Default::default(),
            free_slots: Default::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct SlotMapKey {
    index: u32,
    generation: u32,
}

impl SlotMapKey {
    pub fn index(&self) -> u32 {
        self.index
    }

    pub fn generation(&self) -> u32 {
        self.generation
    }
}

impl Encode for SlotMapKey {
    fn encode(&self, encoder: &mut impl Encoder) -> Result<(), ()> {
        VarInt(self.index).encode(encoder)?;
        VarInt(self.generation).encode(encoder)
    }
}

impl<'a> Decode<'a> for SlotMapKey {
    fn decode(decoder: &mut impl Decoder<'a>) -> Result<Self, ()> {
        Ok(Self {
            index: VarInt::decode(decoder)?.0,
            generation: VarInt::decode(decoder)?.0,
        })
    }
}

impl<T> SlotMap<T> {
    pub fn new() -> Self {
        Self {
            values: vec![],
            generations: vec![],
            free_slots: vec![],
        }
    }

    pub fn len(&self) -> usize {
        self.values.len() - self.free_slots.len()
    }

    pub fn get(&self, key: SlotMapKey) -> Option<&T> {
        self.values
            .get(key.index as usize)
            .map(Option::as_ref)
            .flatten()
            .filter(|_| self.generations[key.index as usize] == key.generation)
    }

    pub fn get_mut(&mut self, key: SlotMapKey) -> Option<&mut T> {
        self.values
            .get_mut(key.index as usize)
            .map(Option::as_mut)
            .flatten()
            .filter(|_| self.generations[key.index as usize] == key.generation)
    }

    pub fn add(&mut self, value: impl FnOnce(SlotMapKey) -> Option<T>) -> SlotMapKey {
        if let Some(free_slot) = self.free_slots.pop() {
            self.generations[free_slot as usize] += 1;
            let key = SlotMapKey {
                index: free_slot,
                generation: self.generations[free_slot as usize],
            };
            self.values[free_slot as usize] = value(key);
            key
        } else {
            let index = self.values.len();
            let generation = if self.generations.len() <= self.values.len() {
                self.generations.push(0);
                0
            } else {
                self.generations[index] += 1;
                self.generations[index]
            };
            let key = SlotMapKey {
                index: index as u32,
                generation,
            };
            self.values.push(value(key));
            key
        }
    }

    pub fn remove(&mut self, key: SlotMapKey) -> Option<T> {
        if self.generations[key.index as usize] != key.generation {
            return None;
        }

        self.values
            .get_mut(key.index as usize)
            .map(Option::take)
            .flatten()
            .inspect(|_| {
                self.free_slots.push(key.index);
            })
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.values.iter().filter_map(Option::as_ref)
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.values.iter_mut().filter_map(Option::as_mut)
    }

    pub fn retain_if(&mut self, mut func: impl FnMut(&mut T) -> bool) {
        for (i, opt_val) in self.values.iter_mut().enumerate() {
            if let Some(val) = opt_val {
                if func(val) {
                    *opt_val = None;
                    self.free_slots.push(i as u32);
                }
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn clear(&mut self) {
        self.values.clear();
        self.free_slots.clear();
    }
}

pub struct HashSlotMap<T> {
    values: HashMap<u32, T>,
    generations: HashMap<u32, u32>,
}

impl<T> HashSlotMap<T> {
    pub fn get(&self, key: SlotMapKey) -> Option<&T> {
        self.valid(key)
            .then(|| self.values.get(&key.index))
            .flatten()
    }

    pub fn get_mut(&mut self, key: SlotMapKey) -> Option<&mut T> {
        self.valid(key)
            .then(|| self.values.get_mut(&key.index))
            .flatten()
    }

    pub fn remove(&mut self, key: SlotMapKey) -> Option<T> {
        self.valid(key)
            .then(|| {
                self.generations
                    .remove(&key.index)
                    .and_then(|_| self.values.remove(&key.index))
            })
            .flatten()
    }

    pub fn insert(&mut self, key: SlotMapKey, value: T) -> Option<T> {
        let old_value = self.remove(key);

        self.generations.insert(key.index, key.generation);
        self.values.insert(key.index, value);

        old_value
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    fn valid(&self, key: SlotMapKey) -> bool {
        match self.generations.get(&key.index) {
            Some(&v) if v == key.generation => true,
            _ => false,
        }
    }
}

#[derive(Default)]
pub struct SlotSet {
    set: HashSet<SlotMapKey>,
}

#[derive(thiserror::Error, Debug)]
pub enum SlotSetInsertError {
    #[error("The key was already inserted into the set")]
    InsertedError,
}

impl SlotSet {
    pub fn insert(&mut self, key: SlotMapKey) -> Result<(), SlotSetInsertError> {
        if !self.set.insert(key) {
            Err(SlotSetInsertError::InsertedError)
        } else {
            Ok(())
        }
    }

    pub fn clear(&mut self){
        self.set.clear();
    }
}
