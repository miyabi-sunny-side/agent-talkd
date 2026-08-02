//! `agent-talk-mcp`: agent が触る唯一の面である stdio MCP server。
//!
//! この binary は会話以外の能力を持たない。file 読み書き・任意 path 指定・subprocess 実行・
//! shell 経由の呼び出しを tool にも実装にも持ち込まないこと
//! (docs/decisions/0001-conversation-broker-scope.md の forbidden effects / premise 5)。

#[path = "../mcp.rs"]
mod mcp;
// daemon と共有する path 導出。MCP は tmux 由来の path だけを使う。
#[allow(dead_code)]
#[path = "../paths.rs"]
mod paths;
// daemon と共有する wire 形式。MCP は構築側だけを使うため一部は未使用になる。
#[allow(dead_code)]
#[path = "../protocol.rs"]
mod protocol;

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    match mcp::serve().await {
        Ok(()) => ExitCode::SUCCESS,
        // 環境が起動時 contract を満たさない場合はここで fail closed する。
        // tool は1つも公開されない。
        Err(error) => {
            eprintln!("agent-talk-mcp: {error}");
            ExitCode::FAILURE
        }
    }
}
