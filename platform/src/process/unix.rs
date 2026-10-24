//! Unix child groups, through process-wrap.
use process_wrap::tokio::{CommandWrap, KillOnDrop, ProcessGroup};
use std::io;
use tokio::process::Command;

/// This OS's child execution.
pub struct Os;

impl super::Process for Os {
    async fn spawn_tree(command: Command) -> io::Result<super::Child> {
        let mut wrap = group(command);
        wrap.wrap(KillOnDrop);
        super::spawn(wrap, true).await
    }

    async fn spawn_detached(mut command: Command) -> io::Result<super::Child> {
        command.kill_on_drop(false);
        super::spawn(group(command), false).await
    }
}

fn group(command: Command) -> CommandWrap {
    let mut wrap = CommandWrap::from(command);
    wrap.wrap(ProcessGroup::leader());
    wrap
}
