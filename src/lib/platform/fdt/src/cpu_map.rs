//! `/cpus/cpu-map`: which processors are threads of one core.
//!
//! The binding (Linux's `Documentation/devicetree/bindings/cpu/cpu-topology`)
//! nests `socketN`, `clusterN`, `coreN` and `threadN` nodes; a `thread` node,
//! or a `core` node with no threads, names its processor by a `cpu` phandle.
//! This reads no more than how many threads each core lists, for `smt=off`'s
//! check on the Arm architectures (`docs/BUILD-FLAGS.md` §3.1 DF4 (b)).
//!
//! The walk is the tree's own depth-first one, bounded as every walk of
//! [`Nodes`] is. A thread that is not a direct child of a core, or that names
//! no processor, makes the map malformed: an error, never "no topology" (the
//! consultant's ledger 2563, condition (3)). Nothing here allocates.

use crate::{Fdt, Node};

/// What [`Fdt::cpu_map`] found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CpuMap {
    /// `core` nodes.
    pub cores: usize,
    /// `thread` nodes.
    pub threads: usize,
    /// The most threads any one core lists: zero where no core lists any
    /// (each core is one processor).
    pub most_a_core: usize,
}

/// Why a `cpu-map` was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CpuMapError {
    /// A `thread` node that is not a direct child of a `core` node.
    ThreadOutsideCore,
    /// A `thread` node with no `cpu` phandle.
    ThreadWithoutCpu,
}

impl core::fmt::Display for CpuMapError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            CpuMapError::ThreadOutsideCore => "cpu-map has a thread outside a core",
            CpuMapError::ThreadWithoutCpu => "cpu-map has a thread that names no processor",
        })
    }
}

/// Whether `node` is called `<kind>` followed by its number.
fn named(node: &Node<'_>, kind: &str) -> bool {
    node.base_name()
        .strip_prefix(kind)
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit()))
}

impl Fdt<'_> {
    /// `/cpus/cpu-map`'s cores and threads, or `None` where the tree has no
    /// map.
    ///
    /// # Errors
    ///
    /// A malformed map ([`CpuMapError`]).
    pub fn cpu_map(&self) -> Result<Option<CpuMap>, CpuMapError> {
        let mut nodes = self.nodes();
        let mut in_cpus = false;
        let mut map_depth = None;
        for node in nodes.by_ref() {
            if node.depth == 1 {
                in_cpus = node.base_name() == "cpus";
            } else if in_cpus && node.depth == 2 && node.base_name() == "cpu-map" {
                map_depth = Some(node.depth);
                break;
            }
        }
        let Some(map_depth) = map_depth else {
            return Ok(None);
        };
        let mut found = CpuMap {
            cores: 0,
            threads: 0,
            most_a_core: 0,
        };
        // The core being read: its depth and its threads so far.
        let mut core: Option<(usize, usize)> = None;
        for node in nodes {
            if node.depth <= map_depth {
                break;
            }
            if core.is_some_and(|(depth, _)| node.depth <= depth) {
                core = None;
            }
            if named(&node, "core") {
                found.cores += 1;
                core = Some((node.depth, 0));
            } else if named(&node, "thread") {
                let Some((depth, threads)) = core.as_mut() else {
                    return Err(CpuMapError::ThreadOutsideCore);
                };
                if node.depth != *depth + 1 {
                    return Err(CpuMapError::ThreadOutsideCore);
                }
                if node.property("cpu").is_none() {
                    return Err(CpuMapError::ThreadWithoutCpu);
                }
                *threads += 1;
                found.threads += 1;
                found.most_a_core = found.most_a_core.max(*threads);
            }
        }
        Ok(Some(found))
    }
}
