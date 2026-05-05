//! Shared ordering validation for history entry sequences.
//!
//! Provides a single helper for verifying that a sequence of entries is
//! strictly increasing by ledger sequence number. Used by the compare,
//! publish, and verify modules to avoid duplicating this invariant check.

/// Details of a strictly-increasing ordering violation.
///
/// The `index` field is **0-based** and refers to the **second element** of the
/// first violating pair — i.e., `entries[index]` is the entry whose sequence
/// value is not greater than `entries[index - 1]`.
pub(crate) struct OrderingViolation {
    /// 0-based index of the violating entry (the second of the pair).
    pub index: usize,
    /// The sequence value of the entry at `index - 1`.
    pub prev_seq: u32,
    /// The sequence value of the entry at `index`.
    pub curr_seq: u32,
}

/// Checks that values extracted via `seq_fn` are strictly increasing.
///
/// Returns `None` if the ordering is valid, or `Some(OrderingViolation)` with
/// details of the first violation found. When multiple violations exist, only
/// the first (lowest index) is reported.
pub(crate) fn find_ordering_violation<T>(
    entries: &[T],
    seq_fn: impl Fn(&T) -> u32,
) -> Option<OrderingViolation> {
    entries.windows(2).enumerate().find_map(|(i, w)| {
        let prev_seq = seq_fn(&w[0]);
        let curr_seq = seq_fn(&w[1]);
        if curr_seq <= prev_seq {
            Some(OrderingViolation {
                index: i + 1,
                prev_seq,
                curr_seq,
            })
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_slice() {
        let entries: Vec<u32> = vec![];
        assert!(find_ordering_violation(&entries, |e| *e).is_none());
    }

    #[test]
    fn test_single_element() {
        assert!(find_ordering_violation(&[42u32], |e| *e).is_none());
    }

    #[test]
    fn test_valid_strictly_increasing() {
        assert!(find_ordering_violation(&[1u32, 2, 3, 4, 5], |e| *e).is_none());
    }

    #[test]
    fn test_equal_seq_is_violation() {
        let v = find_ordering_violation(&[1u32, 2, 3, 3, 5], |e| *e).unwrap();
        assert_eq!(v.index, 3);
        assert_eq!(v.prev_seq, 3);
        assert_eq!(v.curr_seq, 3);
    }

    #[test]
    fn test_decreasing_seq_is_violation() {
        let v = find_ordering_violation(&[1u32, 5, 3, 7], |e| *e).unwrap();
        assert_eq!(v.index, 2);
        assert_eq!(v.prev_seq, 5);
        assert_eq!(v.curr_seq, 3);
    }

    #[test]
    fn test_multiple_violations_reports_first() {
        // Violations at index 1 (5 -> 2) and index 3 (6 -> 4)
        let v = find_ordering_violation(&[5u32, 2, 6, 4], |e| *e).unwrap();
        assert_eq!(v.index, 1);
        assert_eq!(v.prev_seq, 5);
        assert_eq!(v.curr_seq, 2);
    }
}
