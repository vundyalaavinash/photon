mod checkout;
mod diff;
mod functions;
mod gh;
mod github;
mod inbox;
mod store;

pub use checkout::*;
pub use diff::*;
pub use functions::*;
pub use gh::*;
pub use github::*;
pub use inbox::*;
pub use store::*;

use gpui::App;

pub fn init(cx: &mut App) {
    inbox::init_inbox(cx);
}
