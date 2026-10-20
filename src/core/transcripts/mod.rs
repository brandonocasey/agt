//! Claude Code and Codex transcripts: where the files are, whose they are, and the records and
//! turns inside them. `agt sessions` reads them through here for its reports and its index.

mod codex;
pub mod json;
mod locate;
mod records;
mod turns;

pub use locate::{
    Step, Transcript, Walker, all_roots, belongs_to, claude_projects, codex_roots, codex_sessions,
    history_files, is_scratch, session_files,
};
pub use records::Call;
pub use turns::{ScanState, Turn, injected, parse_line};
