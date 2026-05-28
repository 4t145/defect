//! `mode = files`：按 glob 匹配文件名，按 mtime desc 排序后渲染。

use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use defect_config::SearchToolConfig;
use globset::GlobSet;
use ignore::Walk;
use tokio_util::sync::CancellationToken;

use defect_agent::tool::{ToolError, ToolEvent};

use super::{SearchOutput, display_relative, elapsed_ms, make_completed, sort_by_mtime_desc};

#[allow(clippy::too_many_arguments)]
pub(super) fn run(
    walker: Walk,
    glob: GlobSet,
    cwd: &Path,
    head_limit: u32,
    cancel: &CancellationToken,
    config: &SearchToolConfig,
    started: Instant,
) -> ToolEvent {
    let mut hits: Vec<(PathBuf, Option<SystemTime>)> = Vec::new();
    let mut files_scanned: u64 = 0;
    let mut walked: u64 = 0;
    let mut truncated = false;

    for entry in walker {
        if cancel.is_cancelled() {
            return ToolEvent::Failed(ToolError::Canceled);
        }
        walked = walked.saturating_add(1);
        if walked > config.max_walk_files {
            truncated = true;
            break;
        }

        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }

        // glob 既匹配相对工作区的路径，也匹配文件名——LLM 经常用 `**/*.rs`
        // 期望按工作区相对路径匹配，也可能给个 `Cargo.toml` 期望按 basename
        // 匹配。两条都试一下，命中即收。
        let rel = path.strip_prefix(cwd).unwrap_or(path);
        let basename = path.file_name();
        let matched = glob.is_match(rel)
            || glob.is_match(path)
            || basename
                .map(|n| glob.is_match(Path::new(n)))
                .unwrap_or(false);
        if !matched {
            continue;
        }

        files_scanned = files_scanned.saturating_add(1);
        let mtime = entry.metadata().ok().and_then(|m| m.modified().ok());
        hits.push((path.to_path_buf(), mtime));
    }

    sort_by_mtime_desc(&mut hits);
    let total = hits.len() as u32;
    if hits.len() > head_limit as usize {
        truncated = true;
        hits.truncate(head_limit as usize);
    }

    let (text, kept) = render(&hits, cwd, total, truncated, config.max_result_bytes);
    let truncated = truncated || kept < total;
    let output = SearchOutput {
        mode: "files",
        files_scanned,
        files_matched: kept,
        matches_total: kept,
        truncated,
        elapsed_ms: elapsed_ms(started),
        head_limit,
    };
    make_completed(text, output)
}

fn render(
    hits: &[(PathBuf, Option<SystemTime>)],
    cwd: &Path,
    total: u32,
    initial_truncated: bool,
    max_bytes: u64,
) -> (String, u32) {
    if hits.is_empty() {
        return ("(no matches)".to_string(), 0);
    }
    let mut out = String::new();
    let mut byte_truncated = false;
    let mut emitted: u32 = 0;
    for (path, _) in hits {
        let line = format!("{}\n", display_relative(cwd, path));
        if (out.len() as u64).saturating_add(line.len() as u64) > max_bytes {
            byte_truncated = true;
            break;
        }
        out.push_str(&line);
        emitted = emitted.saturating_add(1);
    }
    let truncated = initial_truncated || byte_truncated;
    if truncated {
        out.push_str(&format!(
            "[truncated; showing {emitted} of {total} files]\n"
        ));
    }
    (out, emitted)
}
