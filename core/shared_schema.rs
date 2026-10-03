use std::ops::{Deref, DerefMut};

use arc_swap::ArcSwap;

use crate::schema::Schema;
use crate::sync::{Arc, ArcMutexGuard, Mutex, MutexGuard};

pub(crate) struct SharedSchema {
    lock: Arc<Mutex<Arc<Schema>>>,
    published: Arc<ArcSwap<Schema>>,
}

impl SharedSchema {
    pub(crate) fn new(schema: Arc<Schema>) -> Self {
        Self {
            published: Arc::new(ArcSwap::new(schema.clone())),
            lock: Arc::new(Mutex::new(schema)),
        }
    }

    pub(crate) fn lock(&self) -> SharedSchemaGuard<'_> {
        SharedSchemaGuard {
            guard: self.lock.lock(),
            published: &self.published,
        }
    }

    pub(crate) fn lock_arc(&self) -> OwnedSharedSchemaGuard {
        OwnedSharedSchemaGuard {
            guard: self.lock.lock_arc(),
            published: self.published.clone(),
        }
    }

    pub(crate) fn published(&self) -> arc_swap::Guard<Arc<Schema>> {
        self.published.load()
    }
}

pub(crate) struct SharedSchemaGuard<'a> {
    guard: MutexGuard<'a, Arc<Schema>>,
    published: &'a ArcSwap<Schema>,
}

impl Deref for SharedSchemaGuard<'_> {
    type Target = Arc<Schema>;

    fn deref(&self) -> &Arc<Schema> {
        &self.guard
    }
}

impl DerefMut for SharedSchemaGuard<'_> {
    fn deref_mut(&mut self) -> &mut Arc<Schema> {
        &mut self.guard
    }
}

impl Drop for SharedSchemaGuard<'_> {
    fn drop(&mut self) {
        publish(self.published, &self.guard);
    }
}

pub(crate) struct OwnedSharedSchemaGuard {
    guard: ArcMutexGuard<Arc<Schema>>,
    published: Arc<ArcSwap<Schema>>,
}

impl Deref for OwnedSharedSchemaGuard {
    type Target = Arc<Schema>;

    fn deref(&self) -> &Arc<Schema> {
        &self.guard
    }
}

impl DerefMut for OwnedSharedSchemaGuard {
    fn deref_mut(&mut self) -> &mut Arc<Schema> {
        &mut self.guard
    }
}

impl Drop for OwnedSharedSchemaGuard {
    fn drop(&mut self) {
        publish(&self.published, &self.guard);
    }
}

fn publish(published: &ArcSwap<Schema>, schema: &Arc<Schema>) {
    if !Arc::ptr_eq(&published.load(), schema) {
        published.store(schema.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema_of_version(schema_version: u32) -> Arc<Schema> {
        Arc::new(Schema {
            schema_version,
            ..Schema::default()
        })
    }

    #[test]
    fn a_reader_sees_the_schema_a_guard_left_once_the_guard_is_gone() {
        let shared = SharedSchema::new(schema_of_version(1));
        let mut guard = shared.lock();
        *guard = schema_of_version(2);
        assert_eq!(shared.published().schema_version, 1);
        drop(guard);
        assert_eq!(shared.published().schema_version, 2);
        assert!(Arc::ptr_eq(&shared.published(), &shared.lock()));
    }

    #[test]
    fn a_schema_changed_in_place_through_a_guard_is_published_as_a_new_one() {
        let shared = SharedSchema::new(schema_of_version(1));
        let before = shared.published().clone();
        let mut guard = shared.lock_arc();
        Schema::try_make_mut(&mut guard).unwrap().schema_version = 3;
        drop(guard);
        assert_eq!(before.schema_version, 1);
        assert_eq!(shared.published().schema_version, 3);
        assert!(Arc::ptr_eq(&shared.published(), &shared.lock()));
    }
}
