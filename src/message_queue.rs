use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Mutex, RwLock},
};

use futures::channel::oneshot;
use rbx_dom_weak::{types::Ref, Ustr};

use crate::snapshot::{AppliedPatchSet, RojoTree, SourceTransform};

const CAUSAL_EFFECT_LIMIT: usize = 1024;
const CAUSAL_EFFECT_KEY_LIMIT: usize = 65_536;
const CAUSAL_ACK_LIMIT: usize = 256;
const SOURCE_BASELINE_LIMIT: usize = 4096;
const SOURCE_BASELINE_BYTES: usize = 16 * 1024 * 1024;

struct CausalEffect {
    sequence: u64,
    cursor: u32,
    origin: MessageOrigin,
    structural: HashSet<Ref>,
    properties: HashSet<(Ref, Ustr)>,
}

struct Acknowledgement {
    token: String,
    effects: HashSet<u64>,
}

impl CausalEffect {
    fn covered_by(&self, acknowledgement: Option<&Acknowledgement>) -> bool {
        matches!(self.origin, MessageOrigin::Studio)
            && acknowledgement.is_some_and(|ack| ack.effects.contains(&self.sequence))
    }
}

struct SourceBaseline {
    id: Ref,
    cursor: u32,
    captured_cursor: u32,
    ancestors: Vec<Ref>,
    transform: Option<SourceTransform>,
}

impl SourceBaseline {
    fn bytes(&self) -> usize {
        self.ancestors.len() * std::mem::size_of::<Ref>()
            + self.transform.as_ref().map_or(0, |transform| {
                transform.original.len() + transform.emitted.len()
            })
    }
}

/// Attribute only a unique, accepted Source publication whose explicit Studio
/// origin and final canonical value agree. This examines this write's messages,
/// never queue history, and deliberately drops ambiguous mixed-origin fields.
pub(crate) fn attribute_source_operations(
    entries: &mut [(AppliedPatchSet, MessageOrigin)],
    accepted: &crate::snapshot::PatchSet,
    conflicts: &crate::snapshot::PatchSet,
    tree: &crate::snapshot::RojoTree,
    operation_id: &str,
) {
    use rbx_dom_weak::{types::Variant, ustr};
    use std::collections::HashSet;
    let source = ustr("Source");
    let mut candidates = HashMap::new();
    let mut ambiguous = HashSet::new();
    for update in &accepted.updated_instances {
        if let Some(Some(value @ Variant::String(_))) = update.changed_properties.get(&source) {
            if candidates.insert(update.id, value).is_some() {
                ambiguous.insert(update.id);
            }
        }
    }
    ambiguous.extend(conflicts.removed_instances.iter().copied());
    ambiguous.extend(
        conflicts
            .added_instances
            .iter()
            .map(|addition| addition.instance.snapshot_id),
    );
    for update in &conflicts.updated_instances {
        if update.changed_properties.contains_key(&source)
            || update.changed_parent.is_some()
            || update.changed_name.is_some()
            || update.changed_class_name.is_some()
        {
            ambiguous.insert(update.id);
        }
    }
    let mut occurrences = HashMap::<Ref, usize>::new();
    for (patch, _) in entries.iter_mut() {
        patch.source_operations.clear();
        ambiguous.extend(patch.removed.iter().chain(&patch.added).copied());
        for update in &patch.updated {
            if update.changed_properties.contains_key(&source) {
                *occurrences.entry(update.id).or_default() += 1;
            }
        }
    }
    for (patch, origin) in entries {
        if !matches!(origin, MessageOrigin::Studio) {
            continue;
        }
        for update in &patch.updated {
            let Some(accepted_value) = candidates.get(&update.id) else {
                continue;
            };
            if occurrences.get(&update.id) != Some(&1)
                || std::iter::successors(tree.get_instance(update.id), |instance| {
                    tree.get_instance(instance.parent())
                })
                .any(|instance| ambiguous.contains(&instance.id()))
            {
                continue;
            }
            let Some(Some(published)) = update.changed_properties.get(&source) else {
                continue;
            };
            if published == *accepted_value
                && tree
                    .get_instance(update.id)
                    .is_some_and(|instance| instance.properties().get(&source) == Some(published))
            {
                patch
                    .source_operations
                    .insert(update.id, operation_id.to_owned());
            }
        }
    }
}

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
    filesystem_property_revisions: HashMap<(Ref, Ustr), u32>,
    effects: VecDeque<CausalEffect>,
    effect_keys: usize,
    effect_sequence: u64,
    effects_floor: u32,
    acknowledgements: VecDeque<Acknowledgement>,
    sources: VecDeque<SourceBaseline>,
    source_bytes: usize,
    source_expired_through: Option<u32>,
}

impl History {
    fn push_source_baseline(&mut self, baseline: SourceBaseline) {
        self.source_bytes += baseline.bytes();
        self.sources.push_back(baseline);
        self.trim_source_baselines();
    }

    fn trim_source_baselines(&mut self) {
        while self.sources.len() > SOURCE_BASELINE_LIMIT
            || self.source_bytes > SOURCE_BASELINE_BYTES
        {
            if let Some(expired) = self.sources.pop_front() {
                self.source_bytes -= expired.bytes();
                // An evicted before-image may describe a state much older than its
                // capture. Reject that whole range, including lost ancestry evidence.
                let through = expired.captured_cursor;
                self.source_expired_through = Some(
                    self.source_expired_through
                        .map_or(through, |previous| previous.max(through)),
                );
            }
        }
    }

    fn source_cursor_is_retained(&self, cursor: u32) -> bool {
        cursor <= self.messages.len() as u32
            && cursor >= self.effects_floor
            && self
                .source_expired_through
                .is_none_or(|through| cursor > through)
    }

    fn source_baseline(&self, id: Ref, cursor: u32) -> Option<(SourceTransform, Vec<Ref>)> {
        if !self.source_cursor_is_retained(cursor) {
            return None;
        }
        let baseline = self
            .sources
            .iter()
            .rev()
            .find(|entry| entry.id == id && entry.cursor <= cursor)?;
        let transform = baseline.transform.clone()?;
        // Include departed and intermediate ancestors, not only today's ancestry.
        let ancestors: HashSet<_> = self
            .sources
            .iter()
            .filter(|entry| entry.id == id && entry.cursor >= baseline.cursor)
            .flat_map(|entry| entry.ancestors.iter().copied())
            .collect();
        Some((transform, ancestors.into_iter().collect()))
    }

    fn record_effect(
        &mut self,
        cursor: u32,
        origin: MessageOrigin,
        structural: HashSet<Ref>,
        properties: HashSet<(Ref, Ustr)>,
    ) -> u64 {
        self.effect_sequence += 1;
        let sequence = self.effect_sequence;
        if !structural.is_empty() || !properties.is_empty() {
            self.effect_keys += structural.len() + properties.len();
            self.effects.push_back(CausalEffect {
                sequence,
                cursor,
                origin,
                structural,
                properties,
            });
            while self.effects.len() > CAUSAL_EFFECT_LIMIT
                || self.effect_keys > CAUSAL_EFFECT_KEY_LIMIT
            {
                let expired = self.effects.pop_front().unwrap();
                self.effect_keys -= expired.structural.len() + expired.properties.len();
                self.effects_floor = self.effects_floor.max(expired.cursor);
            }
        }
        sequence
    }
}

#[derive(Clone, Copy)]
pub(crate) enum MessageOrigin {
    Filesystem,
    Studio,
    Correction,
}

pub(crate) fn structural_targets(patch: &AppliedPatchSet) -> impl Iterator<Item = Ref> + '_ {
    patch
        .removed
        .iter()
        .chain(&patch.added)
        .copied()
        .chain(patch.updated.iter().filter_map(|update| {
            (update.changed_name.is_some()
                || update.changed_parent.is_some()
                || update.changed_class_name.is_some()
                || update.changed_metadata.is_some())
            .then_some(update.id)
        }))
}

impl MessageQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_messages(&self, new_messages: &[AppliedPatchSet]) {
        self.push_messages_with_origins(
            new_messages
                .iter()
                .map(|patch| (patch, MessageOrigin::Filesystem)),
            &[],
            &[],
        );
    }

    pub(crate) fn push_messages_with_origins<'a>(
        &self,
        new_messages: impl IntoIterator<Item = (&'a AppliedPatchSet, MessageOrigin)>,
        filesystem_properties: &[(Ref, Ustr)],
        structural_changes: &[Ref],
    ) {
        self.publish(
            new_messages,
            filesystem_properties,
            structural_changes,
            None,
        );
    }

    /// The capability covers only this successful write's Studio effects, never the
    /// cursor span or publications carrying filesystem observations. It is private to
    /// the response and has no relationship to caller-chosen operation IDs.
    pub(crate) fn publish_write<'a>(
        &self,
        new_messages: impl IntoIterator<Item = (&'a AppliedPatchSet, MessageOrigin)>,
        filesystem_properties: &[(Ref, Ustr)],
        structural_changes: &[Ref],
        predecessor: Option<&str>,
        accepted: bool,
    ) -> Option<String> {
        self.publish(
            new_messages,
            filesystem_properties,
            structural_changes,
            accepted.then_some(predecessor),
        )
    }

    fn publish<'a>(
        &self,
        new_messages: impl IntoIterator<Item = (&'a AppliedPatchSet, MessageOrigin)>,
        filesystem_properties: &[(Ref, Ustr)],
        structural_changes: &[Ref],
        acknowledged: Option<Option<&str>>,
    ) -> Option<String> {
        let mut message_listeners = self.message_listeners.lock().unwrap();
        let mut history = self.history.write().unwrap();
        let mut coverage = match acknowledged {
            Some(Some(token)) => history
                .acknowledgements
                .iter()
                .find(|ack| ack.token == token)
                .map(|ack| ack.effects.clone()),
            Some(None) => Some(HashSet::new()),
            None => None,
        };
        let previous_cursor = history.messages.len();
        for (patch, origin) in new_messages {
            let revision = history.messages.len() as u32 + 1;
            if !matches!(origin, MessageOrigin::Correction) {
                for update in &patch.updated {
                    if update
                        .changed_properties
                        .contains_key(&rbx_dom_weak::ustr("Source"))
                    {
                        history.push_source_baseline(SourceBaseline {
                            id: update.id,
                            cursor: revision,
                            captured_cursor: revision,
                            ancestors: Vec::new(),
                            transform: None,
                        });
                    }
                }
                for id in &patch.removed {
                    history.push_source_baseline(SourceBaseline {
                        id: *id,
                        cursor: revision,
                        captured_cursor: revision,
                        ancestors: Vec::new(),
                        transform: None,
                    });
                }
                let sequence = history.record_effect(
                    revision,
                    origin,
                    structural_targets(patch).collect(),
                    patch
                        .updated
                        .iter()
                        .flat_map(|update| {
                            update
                                .changed_properties
                                .keys()
                                .map(move |property| (update.id, *property))
                        })
                        .collect(),
                );
                if matches!(origin, MessageOrigin::Studio) {
                    if let Some(coverage) = &mut coverage {
                        coverage.insert(sequence);
                    }
                }
                for id in structural_targets(patch) {
                    history.structural_revisions.insert(id, revision);
                }
                if matches!(origin, MessageOrigin::Filesystem) {
                    for update in &patch.updated {
                        for property in update.changed_properties.keys() {
                            history
                                .filesystem_property_revisions
                                .insert((update.id, *property), revision);
                        }
                    }
                }
            }
            let mut published = patch.clone();
            if !matches!(origin, MessageOrigin::Studio) {
                published.source_operations.clear();
            }
            history.messages.push(published);
        }
        // Catchups can hide fields a later Studio write replaces. Their provenance must
        // survive even though those intermediate values are never sent back to Studio.
        if (!filesystem_properties.is_empty() || !structural_changes.is_empty())
            && history.messages.len() == previous_cursor
        {
            history.messages.push(AppliedPatchSet::new());
        }
        let revision = history.messages.len() as u32;
        if !filesystem_properties.is_empty() || !structural_changes.is_empty() {
            history.record_effect(
                revision,
                MessageOrigin::Filesystem,
                structural_changes.iter().copied().collect(),
                filesystem_properties.iter().copied().collect(),
            );
        }
        for key in filesystem_properties {
            history.filesystem_property_revisions.insert(*key, revision);
        }
        for id in structural_changes {
            history.structural_revisions.insert(*id, revision);
        }

        let mut remaining_listeners = Vec::new();
        for listener in message_listeners.drain(..) {
            match fire_listener_if_ready(&history.messages, listener) {
                Ok(_) => {}
                Err(listener) => remaining_listeners.push(listener),
            }
        }
        *message_listeners = remaining_listeners;
        coverage.map(|mut effects| {
            let floor = history
                .effects
                .front()
                .map_or(history.effect_sequence, |effect| effect.sequence);
            effects.retain(|sequence| *sequence >= floor);
            let token = uuid::Uuid::new_v4().to_string();
            history.acknowledgements.push_back(Acknowledgement {
                token: token.clone(),
                effects,
            });
            while history.acknowledgements.len() > CAUSAL_ACK_LIMIT {
                history.acknowledgements.pop_front();
            }
            token
        })
    }

    pub(crate) fn valid_predecessor(&self, token: &str, cursor: Option<u32>) -> bool {
        let history = self.history.read().unwrap();
        cursor.is_some_and(|cursor| cursor <= history.messages.len() as u32)
            && history
                .acknowledgements
                .iter()
                .any(|ack| ack.token == token)
    }

    /// Every relevant effect must be in the exact predecessor lineage. Even an
    /// own/foreign/own ABA retains its foreign middle effect until it expires, at
    /// which point the truncated range fails closed instead of inferring ownership.
    pub(crate) fn has_unproven_changes_since(
        &self,
        cursor: u32,
        targets: impl IntoIterator<Item = Ref>,
        properties: impl IntoIterator<Item = (Ref, Ustr)>,
        predecessor: Option<&str>,
    ) -> bool {
        let history = self.history.read().unwrap();
        if cursor > history.messages.len() as u32 || cursor < history.effects_floor {
            return true;
        }
        let coverage = predecessor.and_then(|token| {
            history
                .acknowledgements
                .iter()
                .find(|ack| ack.token == token)
        });
        if predecessor.is_some() && coverage.is_none() {
            return true;
        }
        let targets: HashSet<_> = targets.into_iter().collect();
        let properties: HashSet<_> = properties.into_iter().collect();
        history.effects.iter().any(|effect| {
            effect.cursor > cursor
                && (!effect.structural.is_disjoint(&targets)
                    || !effect.properties.is_disjoint(&properties))
                && !effect.covered_by(coverage)
        })
    }

    pub(crate) fn all_changes_proven_since(&self, cursor: u32, predecessor: &str) -> bool {
        let history = self.history.read().unwrap();
        let Some(ack) = history
            .acknowledgements
            .iter()
            .find(|ack| ack.token == predecessor)
        else {
            return false;
        };
        cursor <= history.messages.len() as u32
            && cursor >= history.effects_floor
            && history
                .effects
                .iter()
                .filter(|effect| effect.cursor > cursor)
                .all(|effect| effect.covered_by(Some(ack)))
    }

    pub(crate) fn source_baseline_ids_captured_at(&self, cursor: u32) -> HashSet<Ref> {
        self.history
            .read()
            .unwrap()
            .sources
            .iter()
            .filter(|entry| entry.captured_cursor == cursor)
            .map(|entry| entry.id)
            .collect()
    }

    /// Capture server-known authored/emitted pairs while the worker holds the tree
    /// lock. Source cursors are independent of the packet's structural cursor.
    #[cfg(test)]
    fn capture_source_baselines(&self, tree: &RojoTree) {
        self.capture_source_baselines_for(
            tree,
            tree.inner()
                .descendants()
                .map(|instance| instance.referent()),
        );
    }

    pub(crate) fn capture_source_baselines_for(
        &self,
        tree: &RojoTree,
        ids: impl IntoIterator<Item = Ref>,
    ) {
        self.capture_source_baselines_at(tree, ids, false);
        let needs_barrier = {
            let history = self.history.read().unwrap();
            history
                .source_expired_through
                .is_some_and(|through| through >= history.messages.len() as u32)
        };
        if needs_barrier {
            // Finalization, not a before-capture, exposes this fresh read cursor.
            // Older drafts stay expired without requiring unrelated filesystem traffic.
            let barrier = AppliedPatchSet::new();
            self.push_messages_with_origins([(&barrier, MessageOrigin::Correction)], &[], &[]);
        }
    }

    /// Keep the first before-image while one unpublished mutation visits an ID more
    /// than once. Intermediate transaction states are not editor-visible baselines.
    pub(crate) fn capture_source_baselines_before_for(
        &self,
        tree: &RojoTree,
        ids: impl IntoIterator<Item = Ref>,
    ) {
        self.capture_source_baselines_at(tree, ids, true);
    }

    fn capture_source_baselines_at(
        &self,
        tree: &RojoTree,
        ids: impl IntoIterator<Item = Ref>,
        before: bool,
    ) {
        let mut history = self.history.write().unwrap();
        let cursor = history.messages.len() as u32;
        let source_key = rbx_dom_weak::ustr("Source");
        for id in ids {
            let Some(instance) = tree.get_instance(id) else {
                continue;
            };
            let Some(rbx_dom_weak::types::Variant::String(source)) =
                instance.properties().get(&source_key)
            else {
                continue;
            };
            let ancestors: Vec<_> = std::iter::successors(Some(instance), |instance| {
                tree.get_instance(instance.parent())
            })
            .map(|instance| instance.id())
            .collect();
            let last = history.sources.iter().rev().find(|entry| entry.id == id);
            if before && last.is_some_and(|entry| entry.captured_cursor == cursor) {
                let last = history
                    .sources
                    .iter_mut()
                    .rev()
                    .find(|entry| entry.id == id)
                    .unwrap();
                let previous_len = last.ancestors.len();
                for ancestor in ancestors {
                    if !last.ancestors.contains(&ancestor) {
                        last.ancestors.push(ancestor);
                    }
                }
                let added_bytes =
                    (last.ancestors.len() - previous_len) * std::mem::size_of::<Ref>();
                history.source_bytes += added_bytes;
                history.trim_source_baselines();
                continue;
            }
            let transform = tree
                .get_metadata(id)
                .and_then(|meta| meta.source_transform.as_ref());
            if transform.is_some_and(|transform| &transform.emitted != source) {
                history.push_source_baseline(SourceBaseline {
                    id,
                    cursor,
                    captured_cursor: cursor,
                    ancestors: Vec::new(),
                    transform: None,
                });
                continue;
            }
            if last.is_some_and(|entry| {
                entry.ancestors == ancestors
                    && entry
                        .transform
                        .as_ref()
                        .is_some_and(|previous| match transform {
                            Some(transform) => previous == transform,
                            None => previous.original == *source && previous.emitted == *source,
                        })
            }) {
                if before {
                    history
                        .sources
                        .iter_mut()
                        .rev()
                        .find(|entry| entry.id == id)
                        .unwrap()
                        .captured_cursor = cursor;
                }
                continue;
            }
            // Fill only the last matching Source publication. Unknown intermediate
            // values remain holes even when a later canonical value is available.
            let published_cursor = last.filter(|entry| entry.transform.is_none()).and_then(|entry| {
                let patch = entry.cursor.checked_sub(1)
                    .and_then(|index| history.messages.get(index as usize))?;
                patch.updated.iter().find(|update| update.id == id)
                    .and_then(|update| update.changed_properties.get(&source_key))
                    .is_some_and(|value| matches!(value, Some(rbx_dom_weak::types::Variant::String(value)) if value == source))
                    .then_some(entry.cursor)
            });
            let source_cursor = if let Some(published_cursor) = published_cursor {
                published_cursor
            } else if last.is_none() {
                // An untouched value was already valid before unrelated traffic.
                // Relevant effects, including own effects, bound that inference.
                history
                    .effects
                    .iter()
                    .filter(|effect| {
                        effect.properties.contains(&(id, source_key))
                            || ancestors
                                .iter()
                                .any(|ancestor| effect.structural.contains(ancestor))
                    })
                    .map(|effect| effect.cursor)
                    .max()
                    .unwrap_or(history.effects_floor)
                    .max(
                        history
                            .source_expired_through
                            .map_or(0, |through| through.saturating_add(1)),
                    )
            } else {
                cursor
            };
            if source_cursor > cursor {
                continue;
            }
            history.push_source_baseline(SourceBaseline {
                id,
                cursor: source_cursor,
                captured_cursor: cursor,
                ancestors,
                transform: Some(match transform {
                    Some(transform) => transform.clone(),
                    None => SourceTransform {
                        original: source.clone(),
                        emitted: source.clone(),
                    },
                }),
            });
        }
    }

    #[cfg(test)]
    fn source_baseline(&self, id: Ref, cursor: u32) -> Option<SourceTransform> {
        self.source_baseline_with_ancestors(id, cursor)
            .map(|(transform, _)| transform)
    }

    #[cfg(test)]
    fn source_baseline_with_ancestors(
        &self,
        id: Ref,
        cursor: u32,
    ) -> Option<(SourceTransform, Vec<Ref>)> {
        self.history.read().unwrap().source_baseline(id, cursor)
    }

    /// The live tree supplies untouched baselines without retaining a duplicate of
    /// every script. A hole or an expired range is never reconstructed from bytes.
    pub(crate) fn source_baseline_or_current(
        &self,
        tree: &RojoTree,
        id: Ref,
        cursor: u32,
    ) -> Option<(SourceTransform, Vec<Ref>)> {
        let history = self.history.read().unwrap();
        if !history.source_cursor_is_retained(cursor) {
            return None;
        }
        if history
            .sources
            .iter()
            .any(|entry| entry.id == id && entry.cursor <= cursor)
        {
            return history.source_baseline(id, cursor);
        }
        let instance = tree.get_instance(id)?;
        let source_key = rbx_dom_weak::ustr("Source");
        let rbx_dom_weak::types::Variant::String(source) =
            instance.properties().get(&source_key)?
        else {
            return None;
        };
        let ancestors: HashSet<_> = std::iter::successors(Some(instance), |instance| {
            tree.get_instance(instance.parent())
        })
        .map(|instance| instance.id())
        .chain(
            history
                .sources
                .iter()
                .filter(|entry| entry.id == id)
                .flat_map(|entry| entry.ancestors.iter().copied()),
        )
        .collect();
        if history.effects.iter().any(|effect| {
            effect.cursor > cursor
                && (effect.properties.contains(&(id, source_key))
                    || !effect.structural.is_disjoint(&ancestors))
        }) {
            return None;
        }
        let transform = match tree
            .get_metadata(id)
            .and_then(|meta| meta.source_transform.as_ref())
        {
            Some(transform) if &transform.emitted == source => transform.clone(),
            Some(_) => return None,
            None => SourceTransform {
                original: source.clone(),
                emitted: source.clone(),
            },
        };
        Some((transform, ancestors.into_iter().collect()))
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

    pub(crate) fn has_filesystem_property_changes_since(
        &self,
        cursor: u32,
        id: Ref,
        properties: impl IntoIterator<Item = Ustr>,
    ) -> Option<bool> {
        let history = self.history.read().unwrap();
        if cursor as usize > history.messages.len() {
            return None;
        }
        Some(properties.into_iter().any(|property| {
            history
                .filesystem_property_revisions
                .get(&(id, property))
                .is_some_and(|revision| *revision > cursor)
        }))
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
    fn source_provenance_requires_accepted_canonical_studio_origin() {
        use crate::snapshot::{InstanceSnapshot, PatchSet, PatchUpdate, RojoTree};
        let mut tree = RojoTree::new(InstanceSnapshot::new());
        let root = tree.get_root_id();
        let ids: Vec<_> = (0..5)
            .map(|_| {
                tree.insert_instance(
                    root,
                    InstanceSnapshot::new()
                        .class_name("ModuleScript")
                        .property("Source", "same"),
                )
            })
            .collect();
        let patch = |id| AppliedPatchSet {
            updated: vec![AppliedPatchUpdate {
                changed_properties: [(ustr("Source"), Some(Variant::String("same".into())))]
                    .into_iter()
                    .collect(),
                ..AppliedPatchUpdate::new(id)
            }],
            ..AppliedPatchSet::new()
        };
        let mut entries = vec![
            (patch(ids[0]), MessageOrigin::Studio),
            (patch(ids[1]), MessageOrigin::Filesystem),
            (patch(ids[2]), MessageOrigin::Studio),
            (patch(ids[3]), MessageOrigin::Studio),
            (patch(ids[4]), MessageOrigin::Filesystem),
            (patch(ids[4]), MessageOrigin::Studio),
        ];
        let accepted = PatchSet {
            updated_instances: [ids[0], ids[1], ids[3], ids[4]]
                .into_iter()
                .map(|id| PatchUpdate {
                    id,
                    changed_name: None,
                    changed_class_name: None,
                    changed_parent: None,
                    changed_metadata: None,
                    changed_properties: [(
                        ustr("Source"),
                        Some(Variant::String(
                            if id == ids[3] { "different" } else { "same" }.into(),
                        )),
                    )]
                    .into_iter()
                    .collect(),
                })
                .collect(),
            ..PatchSet::new()
        };
        attribute_source_operations(&mut entries, &accepted, &PatchSet::new(), &tree, "op_1");
        assert_eq!(
            entries[0]
                .0
                .source_operations
                .get(&ids[0])
                .map(String::as_str),
            Some("op_1")
        );
        // Same bytes do not prove origin. Filesystem, unaccepted refresh, transformed
        // canonical output, and mixed-origin duplicate fields all remain unlabelled.
        assert!(entries[1..]
            .iter()
            .all(|(patch, _)| patch.source_operations.is_empty()));
        let queue = MessageQueue::new();
        queue.push_messages_with_origins(
            entries.iter().map(|(patch, origin)| (patch, *origin)),
            &[],
            &[],
        );
        let (cursor, published) = queue.subscribe(0).try_recv().unwrap().unwrap();
        assert_eq!(cursor, 6);
        assert_eq!(
            published[0].source_operations,
            entries[0].0.source_operations
        );
        let message =
            crate::web::interface::SubscribeMessage::from_patch_update(&tree, published[0].clone());
        let decoded: crate::web::interface::SubscribeMessage =
            rmp_serde::from_slice(&rmp_serde::to_vec_named(&message).unwrap()).unwrap();
        assert_eq!(decoded.source_operations, message.source_operations);
        let restoration = PatchSet {
            removed_instances: vec![root],
            ..PatchSet::new()
        };
        attribute_source_operations(&mut entries, &accepted, &restoration, &tree, "op_2");
        assert!(
            entries
                .iter()
                .all(|(patch, _)| patch.source_operations.is_empty()),
            "Restoring an ancestor makes descendant origin uncertain even with identical bytes"
        );
    }

    #[test]
    fn source_provenance_truth_table_rejects_conflicts_refreshes_and_canonical_drift() {
        use crate::snapshot::{InstanceSnapshot, PatchSet, PatchUpdate, RojoTree};
        for flags in 0..64 {
            let studio = flags & 1 != 0;
            let accepted_source = flags & 2 != 0;
            let published_source = flags & 4 != 0;
            let canonical_matches = flags & 8 != 0;
            let conflict = flags & 16 != 0;
            let duplicate = flags & 32 != 0;
            let tree = RojoTree::new(
                InstanceSnapshot::new()
                    .class_name("ModuleScript")
                    .property("Source", if canonical_matches { "same" } else { "newer" }),
            );
            let id = tree.get_root_id();
            let property = |source| {
                [(
                    ustr(if source { "Source" } else { "Other" }),
                    Some(Variant::String("same".into())),
                )]
                .into_iter()
                .collect()
            };
            let update = |source| PatchUpdate {
                id,
                changed_name: None,
                changed_class_name: None,
                changed_parent: None,
                changed_metadata: None,
                changed_properties: property(source),
            };
            let accepted = PatchSet {
                updated_instances: vec![update(accepted_source)],
                ..PatchSet::new()
            };
            let conflicts = PatchSet {
                updated_instances: if conflict { vec![update(true)] } else { vec![] },
                ..PatchSet::new()
            };
            let patch = AppliedPatchSet {
                updated: vec![AppliedPatchUpdate {
                    changed_properties: property(published_source),
                    ..AppliedPatchUpdate::new(id)
                }],
                ..AppliedPatchSet::new()
            };
            let mut entries = vec![(
                patch.clone(),
                if studio {
                    MessageOrigin::Studio
                } else {
                    MessageOrigin::Filesystem
                },
            )];
            if duplicate {
                entries.push((patch, MessageOrigin::Studio));
            }
            attribute_source_operations(&mut entries, &accepted, &conflicts, &tree, "write_1");
            let expected = studio
                && accepted_source
                && published_source
                && canonical_matches
                && !conflict
                && !duplicate;
            assert_eq!(
                entries[0].0.source_operations.contains_key(&id),
                expected,
                "flags {flags}"
            );
            let queue = MessageQueue::new();
            queue.push_messages_with_origins(
                entries.iter().map(|(patch, origin)| (patch, *origin)),
                &[],
                &[],
            );
            let cursor = queue.cursor();
            // Even an identical patch reused after this write is a filesystem publication,
            // not a causal echo. Default publication clears any stale tokens defensively.
            queue.push_messages(&[entries[0].0.clone()]);
            let (_, independent) = queue.subscribe(cursor).try_recv().unwrap().unwrap();
            assert!(independent[0].source_operations.is_empty());
        }
    }

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
    fn filesystem_property_index_matches_mixed_origin_history() {
        let queue = MessageQueue::new();
        let ids = [Ref::new(), Ref::new()];
        let properties = [ustr("Source"), ustr("Value"), ustr("Unchanged")];
        let mut publications = Vec::new();
        for index in 0..12 {
            let mut update = AppliedPatchUpdate::new(ids[index % 2]);
            update.changed_properties.insert(
                properties[(index / 2) % 2],
                Some(Variant::String(index.to_string())),
            );
            let patch = AppliedPatchSet {
                updated: vec![update],
                ..AppliedPatchSet::new()
            };
            let origin = if index % 3 == 0 {
                MessageOrigin::Filesystem
            } else {
                MessageOrigin::Studio
            };
            publications.push((patch, origin));
        }
        queue.push_messages_with_origins(
            publications.iter().map(|(patch, origin)| (patch, *origin)),
            &[],
            &[],
        );
        for cursor in 0..=publications.len() as u32 {
            for id in ids {
                for mask in 0..8 {
                    let targets: Vec<_> = properties
                        .iter()
                        .enumerate()
                        .filter_map(|(index, property)| {
                            (mask & (1 << index) != 0).then_some(*property)
                        })
                        .collect();
                    let expected = publications[cursor as usize..]
                        .iter()
                        .any(|(patch, origin)| {
                            matches!(origin, MessageOrigin::Filesystem)
                                && patch.updated.iter().any(|update| {
                                    update.id == id
                                        && update
                                            .changed_properties
                                            .keys()
                                            .any(|property| targets.contains(property))
                                })
                        });
                    assert_eq!(
                        queue.has_filesystem_property_changes_since(cursor, id, targets),
                        Some(expected)
                    );
                }
            }
        }
        assert_eq!(
            queue.has_filesystem_property_changes_since(13, ids[0], properties),
            None
        );
        let (_, wire) = queue.subscribe(0).try_recv().unwrap().unwrap();
        assert_eq!(wire.len(), publications.len());
        for (actual, (expected, _)) in wire.iter().zip(&publications) {
            assert_eq!(actual.added, expected.added);
            assert_eq!(actual.removed, expected.removed);
            assert_eq!(actual.updated.len(), expected.updated.len());
            for (actual, expected) in actual.updated.iter().zip(&expected.updated) {
                assert_eq!(actual.id, expected.id);
                assert_eq!(actual.changed_properties, expected.changed_properties);
            }
        }
    }

    #[test]
    fn own_writes_do_not_grow_filesystem_property_index() {
        let queue = MessageQueue::new();
        let id = Ref::new();
        let property = ustr("Source");
        for value in 0..4096 {
            let mut update = AppliedPatchUpdate::new(id);
            update
                .changed_properties
                .insert(property, Some(Variant::String(value.to_string())));
            let patch = AppliedPatchSet {
                updated: vec![update],
                ..AppliedPatchSet::new()
            };
            queue.push_messages_with_origins([(&patch, MessageOrigin::Studio)], &[], &[]);
            assert_eq!(
                queue.has_filesystem_property_changes_since(0, id, [property]),
                Some(false)
            );
        }
        assert!(queue
            .history
            .read()
            .unwrap()
            .filesystem_property_revisions
            .is_empty());
        for _ in 0..8 {
            queue.push_messages_with_origins([], &[(id, property)], &[]);
        }
        assert_eq!(
            queue
                .history
                .read()
                .unwrap()
                .filesystem_property_revisions
                .len(),
            1
        );
        assert_eq!(
            queue.has_filesystem_property_changes_since(4096, id, [property]),
            Some(true)
        );
    }

    #[test]
    fn hidden_catchups_advance_revision_without_exposing_payloads() {
        let queue = MessageQueue::new();
        let id = Ref::new();
        queue.push_messages_with_origins([], &[(id, ustr("Source"))], &[id]);
        assert_eq!(queue.cursor(), 1);
        assert_eq!(queue.has_structural_changes_since(0, [id]), Some(true));
        assert_eq!(
            queue.has_filesystem_property_changes_since(0, id, [ustr("Source")]),
            Some(true)
        );
        assert_eq!(
            queue.has_filesystem_property_changes_since(1, id, [ustr("Source")]),
            Some(false)
        );
        let (_, wire) = queue.subscribe(0).try_recv().unwrap().unwrap();
        assert_eq!(wire.len(), 1);
        assert!(wire[0].is_empty());
    }

    #[test]
    fn causal_capabilities_cover_exact_effects_not_acknowledgement_spans() {
        let queue = MessageQueue::new();
        let actor = Ref::new();
        let code = Ref::new();
        let unrelated = Ref::new();
        let source = ustr("Source");
        let moved = AppliedPatchSet {
            updated: vec![AppliedPatchUpdate {
                changed_parent: Some(Ref::new()),
                ..AppliedPatchUpdate::new(actor)
            }],
            ..AppliedPatchSet::new()
        };
        let first = queue
            .publish_write([(&moved, MessageOrigin::Studio)], &[], &[], None, true)
            .unwrap();
        assert!(!queue.has_unproven_changes_since(0, [actor], [(code, source)], Some(&first)));
        queue.push_messages_with_origins([], &[(unrelated, source)], &[]);
        let edited = AppliedPatchSet {
            updated: vec![AppliedPatchUpdate {
                changed_properties: [(source, Some(Variant::String("own".into())))]
                    .into_iter()
                    .collect(),
                ..AppliedPatchUpdate::new(code)
            }],
            ..AppliedPatchSet::new()
        };
        let next = queue
            .publish_write(
                [(&edited, MessageOrigin::Studio)],
                &[],
                &[],
                Some(&first),
                true,
            )
            .unwrap();
        assert!(!queue.has_unproven_changes_since(0, [actor], [(code, source)], Some(&next)));
        assert!(queue.has_unproven_changes_since(0, [actor], [(unrelated, source)], Some(&next)));
        assert!(!queue.all_changes_proven_since(0, &next));
        assert!(queue.has_unproven_changes_since(0, [actor], [], None));
        assert!(!MessageQueue::new().valid_predecessor(&next, Some(0)));
        assert!(!queue.valid_predecessor(&next, Some(queue.cursor() + 1)));
        let (_, published) = queue.subscribe(0).try_recv().unwrap().unwrap();
        let wire = serde_json::to_string(&published).unwrap();
        assert!(!wire.contains(&first));
        assert!(!wire.contains(&next));
    }

    #[test]
    fn causal_own_foreign_own_aba_and_hidden_observations_remain_blocking() {
        for structural in [false, true] {
            for hidden in [false, true] {
                let queue = MessageQueue::new();
                let id = Ref::new();
                let source = ustr("Source");
                let patch = AppliedPatchSet {
                    updated: vec![AppliedPatchUpdate {
                        changed_parent: structural.then(Ref::new),
                        changed_properties: if structural {
                            Default::default()
                        } else {
                            [(source, Some(Variant::String("same".into())))]
                                .into_iter()
                                .collect()
                        },
                        ..AppliedPatchUpdate::new(id)
                    }],
                    ..AppliedPatchSet::new()
                };
                let first = queue
                    .publish_write([(&patch, MessageOrigin::Studio)], &[], &[], None, true)
                    .unwrap();
                if hidden {
                    queue.push_messages_with_origins([], &[(id, source)], &[id]);
                } else {
                    // Another writer has its own valid chain, but it is not ours.
                    let foreign = queue
                        .publish_write([(&patch, MessageOrigin::Studio)], &[], &[], None, true)
                        .unwrap();
                    assert_ne!(foreign, first);
                }
                let next = queue
                    .publish_write(
                        [(&patch, MessageOrigin::Studio)],
                        &[],
                        &[],
                        Some(&first),
                        true,
                    )
                    .unwrap();
                assert!(queue.has_unproven_changes_since(0, [id], [(id, source)], Some(&next)));
            }
        }
    }

    #[test]
    fn causal_proof_expiry_and_rejected_effects_fail_closed() {
        let queue = MessageQueue::new();
        let id = Ref::new();
        let patch = AppliedPatchSet {
            added: vec![id],
            ..AppliedPatchSet::new()
        };
        let first = queue
            .publish_write([(&patch, MessageOrigin::Studio)], &[], &[], None, true)
            .unwrap();
        assert!(queue
            .publish_write(
                [(&patch, MessageOrigin::Studio)],
                &[],
                &[],
                Some(&first),
                false
            )
            .is_none());
        assert!(queue.has_unproven_changes_since(0, [id], [], Some(&first)));
        assert!(queue
            .publish_write([], &[], &[], Some("unknown"), true)
            .is_none());
        for _ in 0..CAUSAL_ACK_LIMIT {
            queue.publish_write([], &[], &[], None, true).unwrap();
        }
        assert!(!queue.valid_predecessor(&first, Some(0)));
        let mut proof = None;
        for _ in 0..=CAUSAL_EFFECT_LIMIT {
            proof = queue.publish_write(
                [(&patch, MessageOrigin::Studio)],
                &[],
                &[],
                proof.as_deref(),
                true,
            );
        }
        assert!(queue.has_unproven_changes_since(0, [id], [], proof.as_deref()));
        let history = queue.history.read().unwrap();
        assert!(history.effects.len() <= CAUSAL_EFFECT_LIMIT);
        assert!(history.acknowledgements.len() <= CAUSAL_ACK_LIMIT);
        assert!(history.effect_keys <= CAUSAL_EFFECT_KEY_LIMIT);
        drop(history);
        let before = queue.cursor();
        let keys: Vec<_> = (0..=CAUSAL_EFFECT_KEY_LIMIT).map(|_| Ref::new()).collect();
        queue.push_messages_with_origins([], &[], &keys);
        assert!(queue.has_unproven_changes_since(before, [keys[0]], [], proof.as_deref()));
        let history = queue.history.read().unwrap();
        assert!(history.effects.is_empty());
        assert_eq!(history.effect_keys, 0);
        assert_eq!(history.effects_floor, queue.cursor());
    }

    #[test]
    fn source_baselines_use_exact_source_publication_cursors_and_expire_safely() {
        use crate::snapshot::{apply_patch_set, InstanceSnapshot, PatchSet, PatchUpdate};
        let queue = MessageQueue::new();
        let mut tree = RojoTree::new(
            InstanceSnapshot::new()
                .class_name("ModuleScript")
                .property("Source", "before"),
        );
        let id = tree.get_root_id();
        queue.capture_source_baselines(&tree);
        let patch = apply_patch_set(
            &mut tree,
            PatchSet {
                updated_instances: vec![PatchUpdate {
                    id,
                    changed_name: None,
                    changed_class_name: None,
                    changed_parent: None,
                    changed_metadata: None,
                    changed_properties: [(ustr("Source"), Some(Variant::String("after".into())))]
                        .into_iter()
                        .collect(),
                }],
                ..PatchSet::new()
            },
        );
        queue.push_messages(&[patch.clone(), AppliedPatchSet::new()]);
        queue.capture_source_baselines(&tree);
        assert_eq!(queue.cursor(), 2);
        assert_eq!(queue.source_baseline(id, 0).unwrap().emitted, "before");
        assert_eq!(queue.source_baseline(id, 1).unwrap().emitted, "after");
        assert_eq!(queue.source_baseline(id, 2).unwrap().emitted, "after");
        assert!(queue.source_baseline(id, 3).is_none());
        // An intermediate Source whose transform was not captured cannot reuse an
        // older transform just because its cursor falls inside a publication packet.
        let intermediate = AppliedPatchSet {
            updated: vec![AppliedPatchUpdate {
                changed_properties: [(ustr("Source"), Some(Variant::String("unknown".into())))]
                    .into_iter()
                    .collect(),
                ..AppliedPatchUpdate::new(id)
            }],
            ..AppliedPatchSet::new()
        };
        queue.push_messages(&[intermediate, patch]);
        queue.capture_source_baselines(&tree);
        assert!(queue.source_baseline(id, 3).is_none());
        assert_eq!(queue.source_baseline(id, 4).unwrap().emitted, "after");
        let mut history = queue.history.write().unwrap();
        history.push_source_baseline(SourceBaseline {
            id,
            cursor: 4,
            captured_cursor: 4,
            ancestors: vec![id],
            transform: Some(SourceTransform {
                original: "x".repeat(SOURCE_BASELINE_BYTES),
                emitted: "x".into(),
            }),
        });
        assert!(history.sources.is_empty());
        assert_eq!(history.source_bytes, 0);
        drop(history);
        assert!(queue.source_baseline(id, 0).is_none());
    }

    #[test]
    fn first_sparse_source_capture_spans_unrelated_publications() {
        use crate::snapshot::InstanceSnapshot;
        let queue = MessageQueue::new();
        let tree = RojoTree::new(
            InstanceSnapshot::new()
                .class_name("ModuleScript")
                .property("Source", "untouched"),
        );
        let id = tree.get_root_id();
        queue.push_messages(&[AppliedPatchSet {
            added: vec![Ref::new()],
            ..AppliedPatchSet::new()
        }]);
        queue.capture_source_baselines_for(&tree, [id]);
        assert_eq!(queue.source_baseline(id, 0).unwrap().emitted, "untouched");
    }

    #[test]
    fn sparse_source_history_does_not_admit_passive_project_sources() {
        use crate::snapshot::InstanceSnapshot;
        let queue = MessageQueue::new();
        let mut tree = RojoTree::new(InstanceSnapshot::new());
        let root = tree.get_root_id();
        let ids: Vec<_> = (0..7783)
            .map(|_| {
                tree.insert_instance(
                    root,
                    InstanceSnapshot::new()
                        .class_name("ModuleScript")
                        .property("Source", "passive"),
                )
            })
            .collect();
        queue.capture_source_baselines_before_for(&tree, [ids[0], ids[1]]);
        queue.push_messages(&[AppliedPatchSet {
            added: vec![Ref::new()],
            ..AppliedPatchSet::new()
        }]);
        for id in &ids {
            assert_eq!(
                queue
                    .source_baseline_or_current(&tree, *id, 0)
                    .unwrap()
                    .0
                    .emitted,
                "passive"
            );
        }
        let history = queue.history.read().unwrap();
        assert_eq!(history.sources.len(), 2);
        assert_eq!(history.source_expired_through, None);
    }

    #[test]
    fn unpublished_source_captures_keep_the_first_before_image() {
        use crate::snapshot::InstanceSnapshot;
        let queue = MessageQueue::new();
        let mut tree = RojoTree::new(
            InstanceSnapshot::new()
                .class_name("ModuleScript")
                .property("Source", "before"),
        );
        let id = tree.get_root_id();
        queue.capture_source_baselines_for(&tree, [id]);
        queue.push_messages(&[AppliedPatchSet::new()]);
        queue.capture_source_baselines_before_for(&tree, [id]);
        tree.get_instance_mut(id)
            .unwrap()
            .properties_mut()
            .insert(ustr("Source"), Variant::String("intermediate".into()));
        queue.capture_source_baselines_before_for(&tree, [id]);
        assert_eq!(queue.source_baseline(id, 0).unwrap().emitted, "before");
        assert_eq!(queue.source_baseline(id, 1).unwrap().emitted, "before");
        assert_eq!(queue.history.read().unwrap().sources.len(), 1);
        let patch = AppliedPatchSet {
            updated: vec![AppliedPatchUpdate {
                changed_properties: [(
                    ustr("Source"),
                    Some(Variant::String("intermediate".into())),
                )]
                .into_iter()
                .collect(),
                ..AppliedPatchUpdate::new(id)
            }],
            ..AppliedPatchSet::new()
        };
        queue.push_messages(&[patch]);
        queue.capture_source_baselines_for(&tree, [id]);
        assert_eq!(
            queue.source_baseline(id, 2).unwrap().emitted,
            "intermediate"
        );
    }

    #[test]
    fn unpublished_before_images_retain_intermediate_ancestry() {
        use crate::snapshot::{apply_patch_set, InstanceSnapshot, PatchSet, PatchUpdate};
        let queue = MessageQueue::new();
        let mut tree = RojoTree::new(InstanceSnapshot::new());
        let root = tree.get_root_id();
        let a = tree.insert_instance(root, InstanceSnapshot::new().name("A"));
        let b = tree.insert_instance(root, InstanceSnapshot::new().name("B"));
        let id = tree.insert_instance(
            a,
            InstanceSnapshot::new()
                .class_name("ModuleScript")
                .property("Source", "before"),
        );
        queue.capture_source_baselines_before_for(&tree, [id]);
        apply_patch_set(
            &mut tree,
            PatchSet {
                updated_instances: vec![PatchUpdate {
                    id,
                    changed_parent: Some(b),
                    changed_name: None,
                    changed_class_name: None,
                    changed_metadata: None,
                    changed_properties: [(
                        ustr("Source"),
                        Some(Variant::String("intermediate".into())),
                    )]
                    .into_iter()
                    .collect(),
                }],
                ..PatchSet::new()
            },
        );
        queue.capture_source_baselines_before_for(&tree, [id]);
        let (transform, ancestors) = queue.source_baseline_with_ancestors(id, 0).unwrap();
        assert_eq!(transform.emitted, "before");
        assert!(ancestors.contains(&a));
        assert!(ancestors.contains(&b));
        let history = queue.history.read().unwrap();
        assert_eq!(history.sources.len(), 1);
        assert_eq!(history.source_bytes, history.sources[0].bytes());
    }

    #[test]
    fn lazy_source_baselines_reject_holes_and_relevant_effects() {
        use crate::snapshot::InstanceSnapshot;
        let queue = MessageQueue::new();
        let tree = RojoTree::new(
            InstanceSnapshot::new()
                .class_name("ModuleScript")
                .property("Source", "same"),
        );
        let id = tree.get_root_id();
        assert!(queue.source_baseline_or_current(&tree, id, 0).is_some());
        assert!(queue.source_baseline_or_current(&tree, id, 1).is_none());
        let patch = AppliedPatchSet {
            updated: vec![AppliedPatchUpdate {
                changed_properties: [(ustr("Source"), Some(Variant::String("same".into())))]
                    .into_iter()
                    .collect(),
                ..AppliedPatchUpdate::new(id)
            }],
            ..AppliedPatchSet::new()
        };
        queue.push_messages_with_origins([(&patch, MessageOrigin::Studio)], &[], &[]);
        assert!(queue.source_baseline_or_current(&tree, id, 0).is_none());
        assert!(queue.source_baseline_or_current(&tree, id, 1).is_none());
        queue.capture_source_baselines_for(&tree, [id]);
        assert!(queue.source_baseline_or_current(&tree, id, 1).is_some());
        assert!(queue.source_baseline_or_current(&tree, id, 0).is_none());
    }

    #[test]
    fn lazy_source_baselines_require_retained_causal_coverage() {
        use crate::snapshot::InstanceSnapshot;
        let queue = MessageQueue::new();
        let tree = RojoTree::new(
            InstanceSnapshot::new()
                .class_name("ModuleScript")
                .property("Source", "untouched"),
        );
        let id = tree.get_root_id();
        for _ in 0..=CAUSAL_EFFECT_LIMIT {
            queue.push_messages(&[AppliedPatchSet {
                added: vec![Ref::new()],
                ..AppliedPatchSet::new()
            }]);
        }
        assert!(queue.source_baseline_or_current(&tree, id, 0).is_none());
        assert_eq!(
            queue
                .source_baseline_or_current(&tree, id, queue.cursor())
                .unwrap()
                .0
                .emitted,
            "untouched"
        );
    }

    #[test]
    fn first_source_before_image_starts_after_relevant_ancestry_effects() {
        use crate::snapshot::InstanceSnapshot;
        let queue = MessageQueue::new();
        let mut tree = RojoTree::new(InstanceSnapshot::new());
        let root = tree.get_root_id();
        let id = tree.insert_instance(
            root,
            InstanceSnapshot::new()
                .class_name("ModuleScript")
                .property("Source", "same"),
        );
        let patch = AppliedPatchSet {
            updated: vec![AppliedPatchUpdate {
                changed_name: Some("Renamed".into()),
                ..AppliedPatchUpdate::new(root)
            }],
            ..AppliedPatchSet::new()
        };
        queue.push_messages_with_origins([(&patch, MessageOrigin::Studio)], &[], &[]);
        queue.push_messages(&[AppliedPatchSet::new()]);
        queue.capture_source_baselines_before_for(&tree, [id]);
        assert!(queue.source_baseline_or_current(&tree, id, 0).is_none());
        assert_eq!(queue.source_baseline(id, 1).unwrap().emitted, "same");
    }

    #[test]
    fn completed_source_pressure_establishes_a_fresh_read_cursor_without_other_traffic() {
        use crate::snapshot::InstanceSnapshot;
        let queue = MessageQueue::new();
        let tree = RojoTree::new(
            InstanceSnapshot::new()
                .class_name("ModuleScript")
                .property("Source", "current"),
        );
        let id = tree.get_root_id();
        queue.capture_source_baselines_before_for(&tree, [id]);
        {
            let mut history = queue.history.write().unwrap();
            for _ in 0..SOURCE_BASELINE_LIMIT {
                history.push_source_baseline(SourceBaseline {
                    id: Ref::new(),
                    cursor: 0,
                    captured_cursor: 0,
                    ancestors: Vec::new(),
                    transform: None,
                });
            }
        }
        let mut listener = queue.subscribe(0);
        queue.capture_source_baselines_for(&tree, std::iter::empty());
        assert_eq!(queue.cursor(), 1);
        assert!(queue.source_baseline_or_current(&tree, id, 0).is_none());
        assert_eq!(
            queue
                .source_baseline_or_current(&tree, id, 1)
                .unwrap()
                .0
                .emitted,
            "current"
        );
        let (cursor, messages) = listener.try_recv().unwrap().unwrap();
        assert_eq!(cursor, 1);
        assert_eq!(messages.len(), 1);
        assert!(messages[0].is_empty());
        assert!(!queue.has_unproven_changes_since(0, [id], [(id, ustr("Source"))], None));
        queue.capture_source_baselines_for(&tree, std::iter::empty());
        assert_eq!(
            queue.cursor(),
            1,
            "A completed pressure boundary emits only one barrier"
        );
        queue.capture_source_baselines_before_for(&tree, [id]);
        assert_eq!(queue.source_baseline(id, 1).unwrap().emitted, "current");
    }

    #[test]
    fn source_count_pressure_blocks_lazy_reconstruction_until_a_fresh_cursor() {
        use crate::snapshot::InstanceSnapshot;
        let queue = MessageQueue::new();
        let tree = RojoTree::new(
            InstanceSnapshot::new()
                .class_name("ModuleScript")
                .property("Source", "current"),
        );
        let id = tree.get_root_id();
        queue.capture_source_baselines_before_for(&tree, [id]);
        queue.push_messages(&[AppliedPatchSet::new(), AppliedPatchSet::new()]);
        queue.capture_source_baselines_before_for(&tree, [id]);
        {
            let mut history = queue.history.write().unwrap();
            for _ in 0..SOURCE_BASELINE_LIMIT {
                history.push_source_baseline(SourceBaseline {
                    id: Ref::new(),
                    cursor: 0,
                    captured_cursor: 2,
                    ancestors: Vec::new(),
                    transform: None,
                });
            }
            assert_eq!(history.sources.len(), SOURCE_BASELINE_LIMIT);
            assert_eq!(history.source_expired_through, Some(2));
        }
        assert!(queue.source_baseline_or_current(&tree, id, 0).is_none());
        assert!(queue.source_baseline_or_current(&tree, id, 2).is_none());
        queue.push_messages(&[AppliedPatchSet::new()]);
        assert_eq!(
            queue
                .source_baseline_or_current(&tree, id, 3)
                .unwrap()
                .0
                .emitted,
            "current"
        );
        queue.capture_source_baselines_before_for(&tree, [id]);
        assert_eq!(queue.source_baseline(id, 3).unwrap().emitted, "current");
        assert!(queue.source_baseline_or_current(&tree, id, 2).is_none());
    }

    #[test]
    fn source_byte_pressure_and_oversized_entries_fail_closed() {
        use crate::snapshot::InstanceSnapshot;
        for oversized in [false, true] {
            let queue = MessageQueue::new();
            let tree = RojoTree::new(
                InstanceSnapshot::new()
                    .class_name("ModuleScript")
                    .property("Source", "current"),
            );
            let id = tree.get_root_id();
            queue.capture_source_baselines_before_for(&tree, [id]);
            {
                let mut history = queue.history.write().unwrap();
                let bytes = if oversized {
                    SOURCE_BASELINE_BYTES + 1
                } else {
                    SOURCE_BASELINE_BYTES / 2 + 1
                };
                for _ in 0..if oversized { 1 } else { 2 } {
                    history.push_source_baseline(SourceBaseline {
                        id: Ref::new(),
                        cursor: 0,
                        captured_cursor: 0,
                        ancestors: Vec::new(),
                        transform: Some(SourceTransform {
                            original: "x".repeat(bytes),
                            emitted: String::new(),
                        }),
                    });
                }
                assert!(history.source_bytes <= SOURCE_BASELINE_BYTES);
                assert!(history.sources.len() <= SOURCE_BASELINE_LIMIT);
                assert_eq!(history.source_expired_through, Some(0));
                if oversized {
                    assert!(history.sources.is_empty());
                }
            }
            assert!(queue.source_baseline_or_current(&tree, id, 0).is_none());
            queue.push_messages(&[AppliedPatchSet::new()]);
            assert!(queue.source_baseline_or_current(&tree, id, 1).is_some());
        }
    }

    #[test]
    fn source_history_keeps_departed_ancestors_and_foreign_aba_effects() {
        use crate::snapshot::{apply_patch_set, InstanceSnapshot, PatchSet, PatchUpdate};
        let queue = MessageQueue::new();
        let mut tree = RojoTree::new(InstanceSnapshot::new());
        let root = tree.get_root_id();
        let a = tree.insert_instance(root, InstanceSnapshot::new().name("A"));
        let b = tree.insert_instance(root, InstanceSnapshot::new().name("B"));
        let id = tree.insert_instance(
            a,
            InstanceSnapshot::new()
                .class_name("ModuleScript")
                .property("Source", "same"),
        );
        queue.capture_source_baselines_before_for(&tree, [id]);
        let movement = |parent| PatchSet {
            updated_instances: vec![PatchUpdate {
                id,
                changed_parent: Some(parent),
                changed_name: None,
                changed_class_name: None,
                changed_metadata: None,
                changed_properties: Default::default(),
            }],
            ..PatchSet::new()
        };
        let first = apply_patch_set(&mut tree, movement(b));
        let proof = queue
            .publish_write([(&first, MessageOrigin::Studio)], &[], &[], None, true)
            .unwrap();
        queue.capture_source_baselines_for(&tree, [id]);
        let foreign = AppliedPatchSet {
            updated: vec![AppliedPatchUpdate {
                changed_name: Some("Foreign".into()),
                ..AppliedPatchUpdate::new(a)
            }],
            ..AppliedPatchSet::new()
        };
        queue.push_messages(&[foreign]);
        queue.capture_source_baselines_before_for(&tree, [id]);
        let back = apply_patch_set(&mut tree, movement(a));
        let proof = queue
            .publish_write(
                [(&back, MessageOrigin::Studio)],
                &[],
                &[],
                Some(&proof),
                true,
            )
            .unwrap();
        queue.capture_source_baselines_for(&tree, [id]);
        let (transform, ancestors) = queue.source_baseline_or_current(&tree, id, 0).unwrap();
        assert_eq!(transform.emitted, "same");
        assert!(ancestors.contains(&a));
        assert!(ancestors.contains(&b));
        assert!(queue.has_unproven_changes_since(
            0,
            ancestors,
            [(id, ustr("Source"))],
            Some(&proof)
        ));
    }

    #[test]
    fn unchanged_corrections_do_not_invent_filesystem_provenance() {
        let queue = MessageQueue::new();
        let id = Ref::new();
        let patch = AppliedPatchSet {
            updated: vec![AppliedPatchUpdate {
                changed_parent: Some(Ref::new()),
                changed_properties: [(ustr("Source"), Some(Variant::String("same".into())))]
                    .into_iter()
                    .collect(),
                ..AppliedPatchUpdate::new(id)
            }],
            ..AppliedPatchSet::new()
        };
        queue.push_messages_with_origins([(&patch, MessageOrigin::Correction)], &[], &[]);
        assert_eq!(queue.has_structural_changes_since(0, [id]), Some(false));
        assert_eq!(
            queue.has_filesystem_property_changes_since(0, id, [ustr("Source")]),
            Some(false)
        );
        assert!(!queue.has_unproven_changes_since(0, [id], [(id, ustr("Source"))], None));
        // A real disk observation remains blocking even when correction bytes match.
        queue.push_messages_with_origins(
            [(&patch, MessageOrigin::Correction)],
            &[(id, ustr("Source"))],
            &[id],
        );
        assert!(queue.has_unproven_changes_since(0, [id], [(id, ustr("Source"))], None));
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
