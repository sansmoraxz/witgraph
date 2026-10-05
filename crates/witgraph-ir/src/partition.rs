//! A partition of `0..n` into disjoint sets (union-find), as compilation
//! groups nodes into islands and loading groups island members into Stores.

/// Disjoint sets over `0..n`, each named by its smallest member.
#[derive(Debug, Clone)]
pub struct Partition {
    parent: Vec<usize>,
}

impl Partition {
    /// Every element in a set of its own.
    pub fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
        }
    }

    /// The smallest member of `x`'s set.
    pub fn find(&mut self, mut x: usize) -> usize {
        // Path halving.
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }

    /// Joins the sets of `a` and `b`.
    pub fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        // The smaller root wins, so a set stays named by its smallest
        // member.
        self.parent[ra.max(rb)] = ra.min(rb);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_set_is_named_by_its_smallest_member() {
        let mut p = Partition::new(5);
        p.union(4, 2);
        p.union(3, 4);
        assert_eq!((p.find(2), p.find(3), p.find(4)), (2, 2, 2));
        p.union(1, 3);
        assert_eq!(p.find(4), 1);
        assert_eq!(p.find(0), 0);
    }
}
