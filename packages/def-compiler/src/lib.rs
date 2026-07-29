//! Def compilation: text [`Definition`]s → typed binary def structs.
//!
//! Depends on `defs` for the lower-level text parsing/symbol evaluation
//! and the binary def structs. This crate owns the [`DefReader`] (which scans a
//! definition body by field name).
//!
//! [`Definition`]: defs::def::text::Definition

pub mod lower;
pub mod reader;

pub use self::reader::{Args, DefReader, DefReaderError, EvalError, Evaluator};

pub use self::lower::{LowerError, flatten_specialization, lower_def};

use std::path::{Path, PathBuf};

pub fn walk_def_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect(dir, &mut out);
    out.sort();
    out
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("def") | Some("tpl")
        ) {
            out.push(path);
        }
    }
}
