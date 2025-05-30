pub mod net;
pub mod proto;
pub mod util;

use bevy::{DefaultPlugins, app::App};

pub fn main() {
    App::new().add_plugins(DefaultPlugins).run();
}
