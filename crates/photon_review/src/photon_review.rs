mod anchoring;
mod checkout;
mod context;
mod diff;
mod functions;
mod gh;
mod github;
mod inbox;
mod review;
mod review_map;
mod run_locally;
mod since_review;
mod store;

pub use anchoring::*;
pub use checkout::*;
pub use context::*;
pub use diff::*;
pub use functions::*;
pub use gh::*;
pub use github::*;
pub use inbox::*;
pub use review::*;
pub use review_map::*;
pub use run_locally::*;
pub use since_review::*;
pub use store::*;

use gpui::App;

pub fn init(cx: &mut App) {
    inbox::init_inbox(cx);
    review::refuse_parent_trust(checkout::review_root(), cx);
}
