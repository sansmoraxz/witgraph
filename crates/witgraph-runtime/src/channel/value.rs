//! Latched last-write-wins value channel.

use witgraph_ir::Val;

/// A latched value slot: holds at most one value, always readable,
/// last-write-wins. A monotonic version counter tracks writes so
/// observers can detect changes.
#[derive(Debug, Clone)]
pub struct ValueSlot {
    value: Option<Val>,
    version: u64,
}

impl ValueSlot {
    /// Creates an empty slot at version 0.
    pub fn new() -> Self {
        Self {
            value: None,
            version: 0,
        }
    }

    /// Writes a value, replacing any previous one and incrementing the
    /// version counter.
    pub fn write(&mut self, val: Val) {
        self.value = Some(val);
        self.version += 1;
    }

    /// Reads the current value, if any.
    pub fn read(&self) -> Option<&Val> {
        self.value.as_ref()
    }

    /// Returns `true` if the slot has been written since the given version.
    pub fn changed_since(&self, version: u64) -> bool {
        self.version > version
    }

    /// The current version counter.
    pub fn version(&self) -> u64 {
        self.version
    }
}

impl Default for ValueSlot {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_slot_reads_none() {
        let slot = ValueSlot::new();
        assert!(slot.read().is_none());
        assert_eq!(slot.version(), 0);
    }

    #[test]
    fn write_then_read() {
        let mut slot = ValueSlot::new();
        slot.write(Val::U32(42));
        assert_eq!(slot.read(), Some(&Val::U32(42)));
        assert_eq!(slot.version(), 1);
    }

    #[test]
    fn last_write_wins() {
        let mut slot = ValueSlot::new();
        slot.write(Val::U32(1));
        slot.write(Val::U32(2));
        assert_eq!(slot.read(), Some(&Val::U32(2)));
        assert_eq!(slot.version(), 2);
    }

    #[test]
    fn changed_since_tracks_writes() {
        let mut slot = ValueSlot::new();
        assert!(!slot.changed_since(0));

        slot.write(Val::Bool(true));
        assert!(slot.changed_since(0));
        assert!(!slot.changed_since(1));

        slot.write(Val::Bool(false));
        assert!(slot.changed_since(1));
        assert!(!slot.changed_since(2));
    }
}
