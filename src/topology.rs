//! Internal topological ordering of the node graph.
//!
//! This module is an implementation detail of [`Graph`](crate::Graph) and is
//! not part of the public API. It provides a pure [Kahn's algorithm] that runs
//! at compile time to produce a dependency-respecting execution order.
//!
//! [Kahn's algorithm]:
//!     https://en.wikipedia.org/wiki/Topological_sorting#Kahn%27s_algorithm

use crate::graph::{NodeId, PortIdx};
use std::collections::VecDeque;

/// A directed edge in the processing graph: an output port feeding an input port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Edge {
    /// Source `(node, output-port)`.
    pub from: (NodeId, PortIdx),
    /// Destination `(node, input-port)`.
    pub to: (NodeId, PortIdx),
}

/// Topologically sorts `num_nodes` nodes connected by `edges` using Kahn's
/// algorithm (BFS with in-degree counting).
///
/// Returns `Ok(order)` for a valid DAG, where `order` is a permutation of
/// `0..num_nodes` in which every node appears after all of its predecessors.
/// Returns `Err(remaining)` if a cycle is detected; `remaining` lists the node
/// ids that could not be resolved (those participating in or reachable into a
/// cycle).
///
/// Disconnected nodes (in-degree zero with no successors) are all included in
/// the returned order. Edges referencing nodes `>= num_nodes` are skipped.
pub(crate) fn topological_sort(
    num_nodes: usize,
    edges: &[Edge],
) -> Result<Vec<NodeId>, Vec<NodeId>> {
    // in_degree[n] = number of incoming edges to node n.
    let mut in_degree = vec![0_usize; num_nodes];
    for edge in edges {
        if edge.to.0 < num_nodes {
            in_degree[edge.to.0] = in_degree[edge.to.0].saturating_add(1);
        }
    }

    // Seed the queue with every zero-in-degree node.
    let mut queue: VecDeque<NodeId> = (0..num_nodes).filter(|&n| in_degree[n] == 0).collect();
    let mut order = Vec::with_capacity(num_nodes);

    while let Some(n) = queue.pop_front() {
        order.push(n);
        for edge in edges {
            if edge.from.0 == n && edge.to.0 < num_nodes {
                let idx = edge.to.0;
                if in_degree[idx] > 0 {
                    in_degree[idx] -= 1;
                    if in_degree[idx] == 0 {
                        queue.push_back(idx);
                    }
                }
            }
        }
    }

    if order.len() == num_nodes {
        Ok(order)
    } else {
        // The unresolved nodes are exactly those with residual in-degree > 0.
        let remaining: Vec<NodeId> = (0..num_nodes).filter(|&n| in_degree[n] > 0).collect();
        Err(remaining)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Returns `true` iff `order` is a permutation of `0..num_nodes` in which the
    /// source of every in-range edge precedes its destination. Used by the
    /// property test below to check the topological invariant generically.
    fn is_valid_topo_order(order: &[NodeId], num_nodes: usize, edges: &[Edge]) -> bool {
        if order.len() != num_nodes {
            return false;
        }
        let mut seen = vec![false; num_nodes];
        for &node in order {
            if node >= num_nodes {
                return false;
            }
            // Element uniqueness (perm check). `seen[node]` indexing is safe: the
            // guard above proved `node < num_nodes == seen.len()`.
            if std::mem::replace(&mut seen[node], true) {
                return false;
            }
        }
        for edge in edges {
            if edge.from.0 < num_nodes && edge.to.0 < num_nodes {
                let Some(p_from) = order.iter().position(|&x| x == edge.from.0) else {
                    return false;
                };
                let Some(p_to) = order.iter().position(|&x| x == edge.to.0) else {
                    return false;
                };
                if p_from >= p_to {
                    return false;
                }
            }
        }
        true
    }

    #[test]
    fn empty_graph_is_ok() {
        let order = topological_sort(0, &[]).unwrap();
        assert!(order.is_empty());
    }

    #[test]
    fn no_edges_preserves_all_nodes() {
        let order = topological_sort(3, &[]).unwrap();
        assert_eq!(order.len(), 3);
        // Identity order when no edges constrain anything.
        assert_eq!(order, vec![0, 1, 2]);
    }

    #[test]
    fn linear_chain_sorts_in_dependency_order() {
        // 0 -> 1 -> 2
        let edges = [
            Edge {
                from: (0, 0),
                to: (1, 0),
            },
            Edge {
                from: (1, 0),
                to: (2, 0),
            },
        ];
        let order = topological_sort(3, &edges).unwrap();
        assert_eq!(order, vec![0, 1, 2]);
    }

    #[test]
    fn diamond_dag_respects_dependencies() {
        //   0
        //  / \
        // 1   2
        //  \ /
        //   3
        let edges = [
            Edge {
                from: (0, 0),
                to: (1, 0),
            },
            Edge {
                from: (0, 0),
                to: (2, 0),
            },
            Edge {
                from: (1, 0),
                to: (3, 0),
            },
            Edge {
                from: (2, 0),
                to: (3, 0),
            },
        ];
        let order = topological_sort(4, &edges).unwrap();
        // 0 first, 3 last; {1,2} in between in either order.
        assert_eq!(order[0], 0);
        assert_eq!(order[3], 3);
        assert!(order[1] == 1 || order[1] == 2);
        assert!(order[2] == 1 || order[2] == 2);
    }

    #[test]
    fn disconnected_nodes_are_all_included() {
        // 0 -> 1, plus disconnected 2 and 3.
        let edges = [Edge {
            from: (0, 0),
            to: (1, 0),
        }];
        let order = topological_sort(4, &edges).unwrap();
        assert_eq!(order.len(), 4);
        let pos0 = order.iter().position(|&n| n == 0).unwrap();
        let pos1 = order.iter().position(|&n| n == 1).unwrap();
        assert!(pos0 < pos1);
        assert!(order.contains(&2));
        assert!(order.contains(&3));
    }

    #[test]
    fn self_loop_is_a_cycle() {
        let edges = [Edge {
            from: (0, 0),
            to: (0, 0),
        }];
        let result = topological_sort(1, &edges);
        let remaining = result.expect_err("self-loop must be a cycle");
        assert_eq!(remaining, vec![0]);
    }

    #[test]
    fn two_node_cycle_is_detected() {
        // 0 -> 1 -> 0
        let edges = [
            Edge {
                from: (0, 0),
                to: (1, 0),
            },
            Edge {
                from: (1, 0),
                to: (0, 0),
            },
        ];
        let result = topological_sort(2, &edges);
        let remaining = result.expect_err("two-node cycle must be detected");
        assert_eq!(remaining.len(), 2);
    }

    #[test]
    fn edges_beyond_num_nodes_are_skipped() {
        // Edge into node 5 with only 2 nodes present — must be skipped, not panic.
        let edges = [Edge {
            from: (0, 0),
            to: (5, 0),
        }];
        let order = topological_sort(2, &edges).unwrap();
        assert_eq!(order.len(), 2);
    }

    // ===== Property test =====
    // A graph whose edges only ever point low-id -> high-id is acyclic by
    // construction, so `topological_sort` must accept it and emit a valid order.

    proptest! {
        /// For any node count `n` in `2..=16` and any set of edges derived so that
        /// `from < to < n` always holds, the sort returns `Ok` and the resulting
        /// order is a valid topological ordering (permutation + edge precedence).
        #[test]
        fn prop_low_to_high_dag_yields_valid_order(
            n in 2usize..=16,
            seed in prop::collection::vec((proptest::num::u8::ANY, proptest::num::u8::ANY), 0..=30),
        ) {
            let mut edges: Vec<Edge> = Vec::new();
            for &(a, b) in &seed {
                // `from` in `0..=(n-2)`, `to` in `(from+1)..=(n-1)` ⇒ from < to < n.
                let from = usize::from(a) % (n - 1);
                let span = n - from - 1;
                let to = from + 1 + usize::from(b) % span;
                edges.push(Edge { from: (from, 0), to: (to, 0) });
            }
            let order = topological_sort(n, &edges).expect("acyclic graph must sort Ok");
            prop_assert!(
                is_valid_topo_order(&order, n, &edges),
                "order {:?} invalid for n={} edges={:?}",
                order,
                n,
                edges
            );
        }
    }
}
