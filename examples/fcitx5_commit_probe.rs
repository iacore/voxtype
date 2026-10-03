//! Probe for the fcitx5 output driver
//!
//! Reports whether the fcitx5-commit addon is reachable, and with an argument
//! commits that text through the driver exactly as dictation would. Needs fcitx5
//! running with the addon, and a focused text field in an application that is an
//! fcitx5 client - otherwise the addon answers `false` and the driver reports
//! that there is nothing to commit to.
//!
//! ```text
//! cargo run --example fcitx5_commit_probe                 # availability only
//! cargo run --example fcitx5_commit_probe -- "今天天气很好"  # commit text
//! ```

use voxtype::output::fcitx5::Fcitx5Output;
use voxtype::output::TextOutput;

#[tokio::main]
async fn main() {
    let driver = Fcitx5Output::new(None);
    println!("available: {}", driver.is_available().await);

    if let Some(text) = std::env::args().nth(1) {
        match driver.output(&text).await {
            Ok(()) => println!("committed {} chars", text.chars().count()),
            Err(error) => println!("commit failed: {error}"),
        }
    }
}
