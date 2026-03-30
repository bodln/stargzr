use stargzr::player;
use std::path::PathBuf;
use std::env;

#[tokio::main]
async fn main() {
    // Try to get media path from environment variable, otherwise use default
    let media_path = env::var("MEDIA_PATH")
        .unwrap_or_else(|_| "media".to_string());
    
    println!("Starting stargzr with media path: {}", media_path);
    
    player::initialize(PathBuf::from(media_path)).await;
}
// DOCUMENT ENTIRE WORK FLOW WITH CODE SNIPPETS