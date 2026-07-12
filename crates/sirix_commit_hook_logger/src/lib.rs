//! sirix_commit_hook_logger — reference commit/read hook plugin for the
//! sirix:plugin@0.1.0 world.
//!
//! * on-pre-commit: logs "txn <revision> has <N> dirty nodes" through the
//!   host log import, at INFO. Always allows the commit.
//! * on-post-read: passes every read node through unchanged.
//!
//! The point of v0.1 is to prove the loader wires up host imports (log)
//! and can drive both hook entrypoints; a real audit hook would filter
//! on database/resource and possibly veto commits.

wit_bindgen::generate!({
    world: "commit-read-hook-plugin",
    path: "wit",
});

use exports::sirix::plugin::commit_read_hook::Guest;
use sirix::plugin::host;
use sirix::plugin::types::{CommitContext, LogLevel, Projection, ReadContext};

struct Component;

impl Guest for Component {
    fn on_pre_commit(ctx: CommitContext) -> Result<(), String> {
        host::log(
            LogLevel::Info,
            &format!(
                "txn {rev} on {db}/{res} has {n} dirty nodes",
                rev = ctx.revision,
                db = ctx.database,
                res = ctx.resource_name,
                n = ctx.dirty_nodes.len(),
            ),
        );
        Ok(())
    }

    fn on_post_read(_ctx: ReadContext) -> Result<Projection, String> {
        Ok(Projection {
            replacement: None,
            redact: false,
        })
    }
}

export!(Component);
