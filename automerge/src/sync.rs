use itertools::Itertools;
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    io,
    io::Write,
};

use crate::{
    decoding, decoding::Decoder, encoding::Encodable, types::HASH_SIZE, ApplyOptions, Automerge,
    AutomergeError, Change, ChangeHash, OpObserver,
};

mod bloom;
mod state;

pub use bloom::BloomFilter;
pub use state::{Have, State};

const MESSAGE_TYPE_SYNC: u8 = 0x42; // first byte of a sync message, for identification

impl Automerge {
    pub fn generate_sync_message(&self, sync_state: &mut State) -> Option<Message> {
        let our_heads = self.get_heads();

        let our_need = self.get_missing_deps(sync_state.their_heads.as_ref().unwrap_or(&vec![]));

        let their_heads_set = if let Some(ref heads) = sync_state.their_heads {
            heads.iter().collect::<HashSet<_>>()
        } else {
            HashSet::new()
        };
        let our_have = if our_need.iter().all(|hash| their_heads_set.contains(hash)) {
            vec![self.make_bloom_filter(sync_state.shared_heads.clone())]
        } else {
            Vec::new()
        };

        if let Some(ref their_have) = sync_state.their_have {
            if let Some(first_have) = their_have.first().as_ref() {
                if !first_have
                    .last_sync
                    .iter()
                    .all(|hash| self.get_change_by_hash(hash).is_some())
                {
                    let reset_msg = Message {
                        heads: our_heads,
                        need: Vec::new(),
                        have: vec![Have::default()],
                        changes: Vec::new(),
                    };
                    return Some(reset_msg);
                }
            }
        }

        let changes_to_send = if let (Some(their_have), Some(their_need)) = (
            sync_state.their_have.as_ref(),
            sync_state.their_need.as_ref(),
        ) {
            self.get_changes_to_send(their_have, their_need)
                .expect("Should have only used hashes that are in the document")
        } else {
            Vec::new()
        };

        let heads_unchanged = sync_state.last_sent_heads == our_heads;

        let heads_equal = if let Some(their_heads) = sync_state.their_heads.as_ref() {
            their_heads == &our_heads
        } else {
            false
        };

        if heads_unchanged && heads_equal && changes_to_send.is_empty() {
            return None;
        }

        // deduplicate the changes to send with those we have already sent and clone it now
        let changes_to_send = changes_to_send
            .into_iter()
            .filter_map(|change| {
                if !sync_state.sent_hashes.contains(&change.hash) {
                    Some(change.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        sync_state.last_sent_heads = our_heads.clone();
        sync_state
            .sent_hashes
            .extend(changes_to_send.iter().map(|c| c.hash));

        let sync_message = Message {
            heads: our_heads,
            have: our_have,
            need: our_need,
            changes: changes_to_send,
        };

        Some(sync_message)
    }

    pub fn receive_sync_message(
        &mut self,
        sync_state: &mut State,
        message: Message,
    ) -> Result<(), AutomergeError> {
        self.receive_sync_message_with::<()>(sync_state, message, ApplyOptions::default())
    }

    pub fn receive_sync_message_with<'a, Obs: OpObserver>(
        &mut self,
        sync_state: &mut State,
        message: Message,
        options: ApplyOptions<'a, Obs>,
    ) -> Result<(), AutomergeError> {
        let before_heads = self.get_heads();

        let Message {
            heads: message_heads,
            changes: message_changes,
            need: message_need,
            have: message_have,
        } = message;

        let changes_is_empty = message_changes.is_empty();
        if !changes_is_empty {
            self.apply_changes_with(message_changes, options)?;
            sync_state.shared_heads = advance_heads(
                &before_heads.iter().collect(),
                &self.get_heads().into_iter().collect(),
                &sync_state.shared_heads,
            );
        }

        // trim down the sent hashes to those that we know they haven't seen
        self.filter_changes(&message_heads, &mut sync_state.sent_hashes)?;

        if changes_is_empty && message_heads == before_heads {
            sync_state.last_sent_heads = message_heads.clone();
        }

        let known_heads = message_heads
            .iter()
            .filter(|head| self.get_change_by_hash(head).is_some())
            .collect::<Vec<_>>();
        if known_heads.len() == message_heads.len() {
            sync_state.shared_heads = message_heads.clone();
            // If the remote peer has lost all its data, reset our state to perform a full resync
            if message_heads.is_empty() {
                sync_state.last_sent_heads = Default::default();
                sync_state.sent_hashes = Default::default();
            }
        } else {
            sync_state.shared_heads = sync_state
                .shared_heads
                .iter()
                .chain(known_heads)
                .copied()
                .unique()
                .sorted()
                .collect::<Vec<_>>();
        }

        sync_state.their_have = Some(message_have);
        sync_state.their_heads = Some(message_heads);
        sync_state.their_need = Some(message_need);

        Ok(())
    }

    fn make_bloom_filter(&self, last_sync: Vec<ChangeHash>) -> Have {
        let new_changes = self
            .get_changes(&last_sync)
            .expect("Should have only used hashes that are in the document");
        let hashes = new_changes.into_iter().map(|change| &change.hash);
        Have {
            last_sync,
            bloom: BloomFilter::from_hashes(hashes),
        }
    }

    fn get_changes_to_send(
        &self,
        have: &[Have],
        need: &[ChangeHash],
    ) -> Result<Vec<&Change>, AutomergeError> {
        if have.is_empty() {
            Ok(need
                .iter()
                .filter_map(|hash| self.get_change_by_hash(hash))
                .collect())
        } else {
            let mut last_sync_hashes = HashSet::new();
            let mut bloom_filters = Vec::with_capacity(have.len());

            for h in have {
                let Have { last_sync, bloom } = h;
                last_sync_hashes.extend(last_sync);
                bloom_filters.push(bloom);
            }
            let last_sync_hashes = last_sync_hashes.into_iter().copied().collect::<Vec<_>>();

            let changes = self.get_changes(&last_sync_hashes)?;

            let mut change_hashes = HashSet::with_capacity(changes.len());
            let mut dependents: HashMap<ChangeHash, Vec<ChangeHash>> = HashMap::new();
            let mut hashes_to_send = HashSet::new();

            for change in &changes {
                change_hashes.insert(change.hash);

                for dep in &change.deps {
                    dependents.entry(*dep).or_default().push(change.hash);
                }

                if bloom_filters
                    .iter()
                    .all(|bloom| !bloom.contains_hash(&change.hash))
                {
                    hashes_to_send.insert(change.hash);
                }
            }

            let mut stack = hashes_to_send.iter().copied().collect::<Vec<_>>();
            while let Some(hash) = stack.pop() {
                if let Some(deps) = dependents.get(&hash) {
                    for dep in deps {
                        if hashes_to_send.insert(*dep) {
                            stack.push(*dep);
                        }
                    }
                }
            }

            let mut changes_to_send = Vec::new();
            for hash in need {
                hashes_to_send.insert(*hash);
                if !change_hashes.contains(hash) {
                    let change = self.get_change_by_hash(hash);
                    if let Some(change) = change {
                        changes_to_send.push(change);
                    }
                }
            }

            for change in changes {
                if hashes_to_send.contains(&change.hash) {
                    changes_to_send.push(change);
                }
            }
            Ok(changes_to_send)
        }
    }
}

/// The sync message to be sent.
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    /// The heads of the sender.
    pub heads: Vec<ChangeHash>,
    /// The hashes of any changes that are being explicitly requested from the recipient.
    pub need: Vec<ChangeHash>,
    /// A summary of the changes that the sender already has.
    pub have: Vec<Have>,
    /// The changes for the recipient to apply.
    pub changes: Vec<Change>,
}

impl Message {
    pub fn encode(self) -> Vec<u8> {
        let mut buf = vec![MESSAGE_TYPE_SYNC];

        encode_hashes(&mut buf, &self.heads);
        encode_hashes(&mut buf, &self.need);
        (self.have.len() as u32).encode_vec(&mut buf);
        for have in self.have {
            encode_hashes(&mut buf, &have.last_sync);
            have.bloom.to_bytes().encode_vec(&mut buf);
        }

        (self.changes.len() as u32).encode_vec(&mut buf);
        for mut change in self.changes {
            change.compress();
            change.raw_bytes().encode_vec(&mut buf);
        }

        buf
    }

    pub fn decode(bytes: &[u8]) -> Result<Message, decoding::Error> {
        let mut decoder = Decoder::new(Cow::Borrowed(bytes));

        let message_type = decoder.read::<u8>()?;
        if message_type != MESSAGE_TYPE_SYNC {
            return Err(decoding::Error::WrongType {
                expected_one_of: vec![MESSAGE_TYPE_SYNC],
                found: message_type,
            });
        }

        let heads = decode_hashes(&mut decoder)?;
        let need = decode_hashes(&mut decoder)?;
        let have_count = decoder.read::<u32>()?;
        let mut have = Vec::with_capacity(have_count as usize);
        for _ in 0..have_count {
            let last_sync = decode_hashes(&mut decoder)?;
            let bloom_bytes: Vec<u8> = decoder.read()?;
            let bloom = BloomFilter::try_from(bloom_bytes.as_slice())?;
            have.push(Have { last_sync, bloom });
        }

        let change_count = decoder.read::<u32>()?;
        let mut changes = Vec::with_capacity(change_count as usize);
        for _ in 0..change_count {
            let change = decoder.read()?;
            changes.push(Change::from_bytes(change)?);
        }

        Ok(Message {
            heads,
            need,
            have,
            changes,
        })
    }
}

fn encode_hashes(buf: &mut Vec<u8>, hashes: &[ChangeHash]) {
    debug_assert!(
        hashes.windows(2).all(|h| h[0] <= h[1]),
        "hashes were not sorted"
    );
    hashes.encode_vec(buf);
}

impl Encodable for &[ChangeHash] {
    fn encode<W: Write>(&self, buf: &mut W) -> io::Result<usize> {
        let head = self.len().encode(buf)?;
        let mut body = 0;
        for hash in self.iter() {
            buf.write_all(&hash.0)?;
            body += hash.0.len();
        }
        Ok(head + body)
    }
}

fn decode_hashes(decoder: &mut Decoder<'_>) -> Result<Vec<ChangeHash>, decoding::Error> {
    let length = decoder.read::<u32>()?;
    let mut hashes = Vec::with_capacity(length as usize);

    for _ in 0..length {
        let hash_bytes = decoder.read_bytes(HASH_SIZE)?;
        let hash = ChangeHash::try_from(hash_bytes).map_err(decoding::Error::BadChangeFormat)?;
        hashes.push(hash);
    }

    Ok(hashes)
}

fn advance_heads(
    my_old_heads: &HashSet<&ChangeHash>,
    my_new_heads: &HashSet<ChangeHash>,
    our_old_shared_heads: &[ChangeHash],
) -> Vec<ChangeHash> {
    let new_heads = my_new_heads
        .iter()
        .filter(|head| !my_old_heads.contains(head))
        .copied()
        .collect::<Vec<_>>();

    let common_heads = our_old_shared_heads
        .iter()
        .filter(|head| my_new_heads.contains(head))
        .copied()
        .collect::<Vec<_>>();

    let mut advanced_heads = HashSet::with_capacity(new_heads.len() + common_heads.len());
    for head in new_heads.into_iter().chain(common_heads) {
        advanced_heads.insert(head);
    }
    let mut advanced_heads = advanced_heads.into_iter().collect::<Vec<_>>();
    advanced_heads.sort();
    advanced_heads
}
