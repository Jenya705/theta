pub mod resend;
pub mod ordering;
pub mod segment;
pub mod channel;
pub mod udp;
pub mod connection;

use bevy::app::{App, Plugin};

pub struct NetPlugin;

impl Plugin for NetPlugin {
    fn build(&self, app: &mut App) {
        
    }
}