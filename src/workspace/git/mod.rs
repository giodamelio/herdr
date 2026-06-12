mod config;
#[cfg(test)]
mod config_tests;
mod discovery;
mod status;
// Reachable beyond `workspace::git` so app-level tests can build the colocated
// jj+git repos the worktree tests need.
#[cfg(test)]
pub(crate) mod test_support;

pub(crate) use self::discovery::automatic_workspace_label;

pub use self::{
    discovery::{
        derive_label_from_cwd, fallback_label_from_cwd, git_branch, git_space_metadata,
        GitSpaceMetadata,
    },
    status::{
        git_status_cache_key, git_status_cache_key_for_space,
        git_status_snapshot_for_cwd_with_demand, GitStatusCacheEntry, GitStatusRefreshDemand,
    },
};
