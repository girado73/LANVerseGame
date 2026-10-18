//! Hands out unique, monotonically increasing marker ids.
//!
//! Marker ids must be unique for the whole match so clients can remove the
//! right marker. The counter lives on the world, not inside the Lua script.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkerSequence {
    next: i64,
}

impl Default for MarkerSequence {
    fn default() -> Self {
        Self { next: 1 }
    }
}

impl MarkerSequence {
    pub fn new() -> Self {
        Self::default()
    }

    /// Hands out the next id and advances the counter.
    pub fn take(&mut self) -> i64 {
        let id = self.next;
        self.next += 1;
        id
    }

    /// Keeps the counter above an id that was imported from elsewhere.
    pub fn observe(&mut self, id: i64) {
        if id >= self.next {
            self.next = id + 1;
        }
    }

    pub fn peek(&self) -> i64 {
        self.next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_monotonic() {
        let mut sequence = MarkerSequence::new();
        assert_eq!(sequence.peek(), 1);
        assert_eq!(sequence.take(), 1);
        assert_eq!(sequence.take(), 2);
        assert_eq!(sequence.peek(), 3);
    }

    #[test]
    fn observe_only_moves_forward() {
        let mut sequence = MarkerSequence::new();
        sequence.observe(10);
        assert_eq!(sequence.peek(), 11);
        sequence.observe(3);
        assert_eq!(sequence.peek(), 11);
    }
}
