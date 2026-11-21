use std::{
    collections::HashMap,
    sync::{Mutex, RwLock},
};

use futures::channel::oneshot;
use rbx_dom_weak::types::Ref;

use crate::snapshot::AppliedPatchSet;

/// A patch queue with persistent history that can be subscribed to.
#[derive(Default)]
pub struct MessageQueue {
    history: RwLock<History>,
    message_listeners: Mutex<Vec<Listener>>,
}

#[derive(Default)]
struct History {
    messages: Vec<AppliedPatchSet>,
    structural_revisions: HashMap<Ref, u32>,
}

impl MessageQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_messages(&self, new_messages: &[AppliedPatchSet]) {
        let mut message_listeners = self.message_listeners.lock().unwrap();
        let mut history = self.history.write().unwrap();
        for patch in new_messages {
            let revision = history.messages.len() as u32 + 1;
            for id in patch.removed.iter().chain(&patch.added).copied().chain(
                patch.updated.iter().filter_map(|update| {
                    (update.changed_name.is_some()
                        || update.changed_parent.is_some()
                        || update.changed_class_name.is_some()
                        || update.changed_metadata.is_some())
                    .then_some(update.id)
                }),
            ) {
                history.structural_revisions.insert(id, revision);
            }
            history.messages.push(patch.clone());
        }

        let mut remaining_listeners = Vec::new();
        for listener in message_listeners.drain(..) {
            match fire_listener_if_ready(&history.messages, listener) {
                Ok(_) => {}
                Err(listener) => remaining_listeners.push(listener),
            }
        }
        *message_listeners = remaining_listeners;
    }

    /// Subscribe to any messages occurring after the given message cursor.
    pub fn subscribe(&self, cursor: u32) -> oneshot::Receiver<(u32, Vec<AppliedPatchSet>)> {
        let (sender, receiver) = oneshot::channel();
        // Match publication's lock order so a push cannot slip between checking history
        // and registering a waiting listener.
        let mut message_listeners = self.message_listeners.lock().unwrap();
        let history = self.history.read().unwrap();
        if let Err(listener) =
            fire_listener_if_ready(&history.messages, Listener { sender, cursor })
        {
            message_listeners.push(listener);
        }
        receiver
    }

    /// Subscribe to any messages being pushed into the queue.
    #[cfg(test)]
    #[allow(unused)]
    pub fn subscribe_any(&self) -> oneshot::Receiver<(u32, Vec<AppliedPatchSet>)> {
        self.subscribe(self.cursor())
    }

    pub fn cursor(&self) -> u32 {
        self.history.read().unwrap().messages.len() as u32
    }

    pub(crate) fn has_structural_changes_since(
        &self,
        cursor: u32,
        targets: impl IntoIterator<Item = Ref>,
    ) -> Option<bool> {
        let history = self.history.read().unwrap();
        if cursor as usize > history.messages.len() {
            return None;
        }
        // Ordinary echoes never enter this index. Query cost depends on the target's
        // ancestry, not how far subscription progress lags behind property writes.
        Some(targets.into_iter().any(|id| {
            history
                .structural_revisions
                .get(&id)
                .is_some_and(|revision| *revision > cursor)
        }))
    }
}

struct Listener {
    sender: oneshot::Sender<(u32, Vec<AppliedPatchSet>)>,
    cursor: u32,
}

fn fire_listener_if_ready(
    messages: &[AppliedPatchSet],
    listener: Listener,
) -> Result<(), Listener> {
    let current_cursor = messages.len() as u32;
    if listener.cursor < current_cursor {
        let new_messages = messages[(listener.cursor as usize)..].to_vec();
        let _ = listener.sender.send((current_cursor, new_messages));
        Ok(())
    } else {
        Err(listener)
    }
}

#[cfg(test)]
mod tests {
    use rbx_dom_weak::{types::Variant, ustr};

    use crate::snapshot::{AppliedPatchSet, AppliedPatchUpdate, InstanceMetadata};

    use super::*;

    #[test]
    fn repeated_property_queries_do_not_revisit_ordinary_history() {
        let queue = MessageQueue::new();
        let id = Ref::new();
        for value in 0..4096 {
            let mut update = AppliedPatchUpdate::new(id);
            update
                .changed_properties
                .insert(ustr("Value"), Some(Variant::Int32(value)));
            queue.push_messages(&[AppliedPatchSet {
                updated: vec![update],
                ..AppliedPatchSet::new()
            }]);
            assert_eq!(queue.has_structural_changes_since(0, [id]), Some(false));
        }
        let history = queue.history.read().unwrap();
        assert_eq!(history.messages.len(), 4096);
        assert!(history.structural_revisions.is_empty());
    }

    #[test]
    fn structural_index_matches_history_at_every_cursor() {
        let queue = MessageQueue::new();
        let ids = [Ref::new(), Ref::new(), Ref::new()];
        let mut patches = Vec::new();
        for kind in 0..7 {
            let mut patch = AppliedPatchSet::new();
            let mut update = AppliedPatchUpdate::new(ids[kind % 2]);
            match kind {
                0 => patch.added.push(ids[0]),
                1 => patch.removed.push(ids[1]),
                2 => update.changed_name = Some("Renamed".into()),
                3 => update.changed_parent = Some(ids[0]),
                4 => update.changed_class_name = Some(ustr("Folder")),
                5 => update.changed_metadata = Some(InstanceMetadata::new()),
                _ => {
                    update
                        .changed_properties
                        .insert(ustr("Value"), Some(Variant::Int32(1)));
                }
            }
            patch.updated.push(update);
            patches.push(patch);
        }
        queue.push_messages(&patches[..3]);
        queue.push_messages(&patches[3..]);
        for cursor in 0..=patches.len() as u32 {
            for mask in 0..8 {
                let targets: Vec<_> = ids
                    .iter()
                    .enumerate()
                    .filter_map(|(index, id)| (mask & (1 << index) != 0).then_some(*id))
                    .collect();
                let expected = patches[cursor as usize..].iter().any(|patch| {
                    patch
                        .added
                        .iter()
                        .chain(&patch.removed)
                        .any(|id| targets.contains(id))
                        || patch.updated.iter().any(|update| {
                            targets.contains(&update.id)
                                && (update.changed_name.is_some()
                                    || update.changed_parent.is_some()
                                    || update.changed_class_name.is_some()
                                    || update.changed_metadata.is_some())
                        })
                });
                for _ in 0..2 {
                    assert_eq!(
                        queue.has_structural_changes_since(cursor, targets.iter().copied()),
                        Some(expected)
                    );
                }
            }
        }
        assert_eq!(queue.has_structural_changes_since(8, ids), None);
        let (cursor, messages) = queue.subscribe(0).try_recv().unwrap().unwrap();
        assert_eq!(cursor, 7);
        assert_eq!(messages.len(), 7);
        assert_eq!(queue.cursor(), 7);
    }

    #[test]
    fn concurrent_subscriptions_do_not_miss_publication() {
        let queue = std::sync::Arc::new(MessageQueue::new());
        for cursor in 0..128 {
            let subscriber = queue.clone();
            let thread = std::thread::spawn(move || subscriber.subscribe(cursor));
            queue.push_messages(&[AppliedPatchSet::new()]);
            let mut receiver = thread.join().unwrap();
            assert_eq!(receiver.try_recv().unwrap().unwrap().0, cursor + 1);
        }
    }
}
