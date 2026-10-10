//! Test support for basal's isolated rig, ckdev-flows (`script/flows-rig.sh`), and
//! nothing else. Two binaries use it:
//!
//! - `ck-callosum-stub`, placed on the rig as `ckdev-callosum`: a module with
//!   the reserved id `callosum`, the only principal prefrontal-core and basal
//!   treat as the operator. It exists only to answer consent cards (and to
//!   carry the few operator-only calls the suite needs) on the rig.
//! - `basal-rig-contract`, run by `script/flows-rig.sh test`: the live
//!   contract suite against the real prefrontal-core.
//!
//! - [`client`]: management calls over the rig's daemon, scoped or not.
//! - [`stores`]: read-only views of core's and basal's stores.
//! - [`flows`]: the manifests and scripts the suite installs.
//! - [`models`]: independent model result, tool and usage evidence checks.

pub mod client;
pub mod flows;
pub mod models;
pub mod packages;
pub mod stores;
