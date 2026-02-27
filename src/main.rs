use stargzr::player;
use std::path::PathBuf;
use std::env;

#[tokio::main]
async fn main() {
    // Try to get music path from environment variable, otherwise use default
    let music_path = env::var("MUSIC_PATH")
        .unwrap_or_else(|_| "D:/Skola/.projekti/stargzr/music".to_string());
    
    println!("Starting stargzr with music path: {}", music_path);
    
    player::initialize(PathBuf::from(music_path)).await;
}
// DOCUMENT ENTIRE WORK FLOW WITH CODE SNIPPETS