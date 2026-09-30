//! One-shot future resolution channel.

use crate::error::ChannelError;
use witgraph_ir::Val;

/// A one-shot slot that resolves exactly once.
///
/// [`resolve()`](FutureSlot::resolve) stores the value;
/// [`poll()`](FutureSlot::poll) reads it without consuming. A second
/// resolve returns [`ChannelError::AlreadyResolved`].
#[derive(Debug, Clone)]
pub struct FutureSlot {
    value: Option<Val>,
}

impl FutureSlot {
    /// Creates an unresolved slot.
    pub fn new() -> Self {
        Self { value: None }
    }

    /// Resolves the future with the given value. Returns
    /// [`ChannelError::AlreadyResolved`] on a second call.
    pub fn resolve(&mut self, val: Val) -> Result<(), ChannelError> {
        if self.value.is_some() {
            return Err(ChannelError::AlreadyResolved);
        }
        self.value = Some(val);
        Ok(())
    }

    /// Polls the resolved value, if any.
    pub fn poll(&self) -> Option<&Val> {
        self.value.as_ref()
    }

    /// Returns `true` if the future has been resolved.
    pub fn is_resolved(&self) -> bool {
        self.value.is_some()
    }
}

impl Default for FutureSlot {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unresolved_polls_none() {
        let slot = FutureSlot::new();
        assert!(slot.poll().is_none());
        assert!(!slot.is_resolved());
    }

    #[test]
    fn resolve_then_poll() {
        let mut slot = FutureSlot::new();
        slot.resolve(Val::String("done".into())).unwrap();
        assert_eq!(slot.poll(), Some(&Val::String("done".into())));
        assert!(slot.is_resolved());
    }

    #[test]
    fn double_resolve_rejected() {
        let mut slot = FutureSlot::new();
        slot.resolve(Val::U32(1)).unwrap();
        let err = slot.resolve(Val::U32(2)).unwrap_err();
        assert!(matches!(err, ChannelError::AlreadyResolved));
        assert_eq!(
            slot.poll(),
            Some(&Val::U32(1)),
            "original value preserved after rejected double-resolve"
        );
    }

    #[test]
    fn poll_is_non_consuming() {
        let mut slot = FutureSlot::new();
        slot.resolve(Val::Bool(true)).unwrap();
        assert_eq!(slot.poll(), Some(&Val::Bool(true)));
        assert_eq!(slot.poll(), Some(&Val::Bool(true)));
    }
}
