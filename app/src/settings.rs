use std::sync::{Arc, Mutex};

#[derive(Debug)]
pub struct Settings {
    pub quickwit_url: String,
    pub quickwit_index: String,
}

pub type SharedSettings = Arc<Mutex<Settings>>;
