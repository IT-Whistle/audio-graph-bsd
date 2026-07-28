//! Graph construction and compilation errors.

use crate::graph::{NodeId, PortIdx};
use thiserror::Error;

/// Errors raised while building or compiling an audio [`Graph`](crate::Graph).
///
/// Every variant describes a construction- or compile-time failure. None of
/// these errors can originate on the real-time thread: link validation and
/// topological sorting both run in [`Graph::compile`](crate::Graph::compile),
/// and the only RT-path error is [`GraphError::NotCompiled`] (a single branch).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GraphError {
    /// The graph contains a cycle, so no valid processing order exists.
    ///
    /// The accompanying node ids are those that remain after Kahn's algorithm
    /// could make no further progress — i.e. the nodes participating in (or
    /// reachable into) the cycle.
    #[error("cycle detected in graph; unresolved nodes: {nodes:?}")]
    CycleDetected {
        /// Node ids that could not be ordered (part of / feeding a cycle).
        nodes: Vec<NodeId>,
    },

    /// A referenced port does not exist on the named node.
    #[error("port {port} not found on node {node}")]
    PortNotFound {
        /// The node that was addressed.
        node: NodeId,
        /// The port index that does not exist.
        port: PortIdx,
    },

    /// Two ports with incompatible directions were linked.
    ///
    /// A valid edge connects an **output** port to an **input** port.
    #[error("port direction mismatch: {from:?} -> {to:?} (expected Output -> Input)")]
    PortDirectionMismatch {
        /// The `(node, port)` that should have been an output.
        from: (NodeId, PortIdx),
        /// The `(node, port)` that should have been an input.
        to: (NodeId, PortIdx),
    },

    /// Two ports with mismatched channel counts or sample formats were linked.
    #[error("ports incompatible: {from:?} -> {to:?} (channels or sample format differ)")]
    PortIncompatible {
        /// The upstream output `(node, port)`.
        from: (NodeId, PortIdx),
        /// The downstream input `(node, port)`.
        to: (NodeId, PortIdx),
    },

    /// [`Graph::process_cycle`](crate::Graph::process_cycle) was called before
    /// the graph was compiled.
    #[error("graph is not compiled; call Graph::compile first")]
    NotCompiled,

    /// [`Graph::compile`](crate::Graph::compile) was called on an
    /// already-compiled graph.
    #[error("graph is already compiled")]
    AlreadyCompiled,

    /// A referenced node id does not exist in the graph.
    #[error("node {0} not found")]
    NodeNotFound(NodeId),
}

#[cfg(test)]
mod tests {
    use super::GraphError;

    #[test]
    fn cycle_detected_display_lists_nodes() {
        let e = GraphError::CycleDetected { nodes: vec![2, 3] };
        assert_eq!(
            e.to_string(),
            "cycle detected in graph; unresolved nodes: [2, 3]"
        );
    }

    #[test]
    fn port_not_found_display() {
        let e = GraphError::PortNotFound { node: 1, port: 2 };
        assert_eq!(e.to_string(), "port 2 not found on node 1");
    }

    #[test]
    fn port_direction_mismatch_display() {
        let e = GraphError::PortDirectionMismatch {
            from: (0, 1),
            to: (2, 0),
        };
        assert!(e.to_string().contains("Output -> Input"));
    }

    #[test]
    fn port_incompatible_display() {
        let e = GraphError::PortIncompatible {
            from: (0, 0),
            to: (1, 0),
        };
        assert!(e.to_string().contains("incompatible"));
    }

    #[test]
    fn not_compiled_display() {
        let e = GraphError::NotCompiled;
        assert_eq!(
            e.to_string(),
            "graph is not compiled; call Graph::compile first"
        );
    }

    #[test]
    fn already_compiled_display() {
        let e = GraphError::AlreadyCompiled;
        assert_eq!(e.to_string(), "graph is already compiled");
    }

    #[test]
    fn node_not_found_display() {
        let e = GraphError::NodeNotFound(5);
        assert_eq!(e.to_string(), "node 5 not found");
    }

    #[test]
    fn clone_and_eq_hold() {
        let a = GraphError::NodeNotFound(3);
        assert_eq!(a.clone(), a);
        assert_ne!(GraphError::NodeNotFound(1), GraphError::NotCompiled);
    }
}
