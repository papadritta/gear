// This file is part of Gear.

// Copyright (C) 2022 Gear Technologies Inc.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use codec::Codec;
use core::{fmt::Debug, iter::Iterator, marker::PhantomData};
use gear_core::{
    ids::MessageId,
    message::{StoredDispatch, StoredMessage},
};

pub trait GetKey {
    type Key;

    fn key(&self) -> Self::Key;
}

impl GetKey for StoredMessage {
    type Key = MessageId;

    fn key(&self) -> Self::Key {
        self.id()
    }
}

impl GetKey for StoredDispatch {
    type Key = MessageId;

    fn key(&self) -> Self::Key {
        self.id()
    }
}

pub struct Node<K, V> {
    pub next: Option<K>,
    pub value: V,
}

pub trait MapQueue: Sized {
    type Key: Clone + Debug;
    type Value: Codec + GetKey<Key = Self::Key>;

    fn head_key() -> Option<Self::Key>;
    fn remove_head_key() -> Option<Self::Key>;
    fn set_head_key(key: Self::Key) -> Option<Self::Key>;

    fn tail_key() -> Option<Self::Key>;
    fn remove_tail_key() -> Option<Self::Key>;
    fn set_tail_key(key: Self::Key) -> Option<Self::Key>;

    fn contains(key: &Self::Key) -> bool;
    fn get(key: &Self::Key) -> Option<Node<Self::Key, Self::Value>>;
    fn set(value: Node<Self::Key, Self::Value>) -> Option<Node<Self::Key, Self::Value>>;
    fn remove(key: Self::Key) -> Option<Node<Self::Key, Self::Value>>;

    fn set_head(value: Self::Value) {
        let key = value.key();

        #[cfg(test)]
        assert!(!Self::contains(&key));

        let value = match Self::head_key() {
            Some(head_key) => {
                #[cfg(test)]
                assert!(Self::contains(&head_key));

                Self::set_head_key(key);

                Node {
                    next: Some(head_key),
                    value,
                }
            }
            _ => {
                #[cfg(test)]
                assert!(Self::tail_key().is_none());

                Self::set_head_key(key.clone());
                Self::set_tail_key(key);

                Node { next: None, value }
            }
        };

        Self::set(value);
    }

    fn enqueue(value: Self::Value) {
        #[cfg(test)]
        assert!(!Self::contains(&value.key()));

        if let Some(tail_key) = Self::tail_key() {
            let mut prev_tail =
                Self::get(&tail_key).expect("Unreachable. Checked in getting tail key.");

            #[cfg(test)]
            assert!(prev_tail.next.is_none());

            prev_tail.next = Some(value.key());

            Self::set(prev_tail);

            Self::set_tail_key(value.key());

            let value = Node { next: None, value };

            Self::set(value);
        } else {
            #[cfg(test)]
            assert!(Self::head_key().is_none());

            Self::set_head(value);
        }
    }

    fn dequeue() -> Option<Self::Value> {
        if let Some(head_key) = Self::remove_head_key() {
            let prev_head = Self::remove(head_key).expect("Should be unreachable if head key set.");

            if let Some(next_key) = prev_head.next {
                Self::set_head_key(next_key);
            } else {
                Self::remove_tail_key();
            };

            Some(prev_head.value)
        } else {
            #[cfg(test)]
            assert!(Self::tail_key().is_none());

            None
        }
    }

    fn iter() -> MapQueueIterator<Self::Key, Self::Value, Self> {
        MapQueueIterator(Self::head_key(), Default::default())
    }
}

pub struct MapQueueIterator<K, V, M>(Option<K>, PhantomData<(V, M)>)
where
    K: Clone,
    V: Codec + GetKey<Key = K>,
    M: MapQueue<Key = K, Value = V>;

impl<K, V, M> Iterator for MapQueueIterator<K, V, M>
where
    K: Clone,
    V: Codec + GetKey<Key = K>,
    M: MapQueue<Key = K, Value = V>,
{
    type Item = V;

    fn next(&mut self) -> Option<Self::Item> {
        let current_head = self.0.as_ref()?;
        let current_node = M::get(current_head)?;

        self.0 = current_node.next;

        Some(current_node.value)
    }
}
