use stargzr::player;
use std::path::PathBuf;
use std::env;

#[tokio::main]
async fn main() {
    // Try to get music path from environment variable, otherwise use default
    let music_path = env::var("MUSIC_PATH")
        .unwrap_or_else(|_| "music".to_string());
    
    println!("Starting stargzr with music path: {}", music_path);
    
    player::initialize(PathBuf::from(music_path)).await;
}
// DOCUMENT ENTIRE WORK FLOW WITH CODE SNIPPETS

// TODO
// Change names in here, bacause just changing stuff from mucic to media somehow broke the playlist


// TODO
// When broadcasting from mobile and swtich between media thats different types the listeners dont makethat change, example listening a song and then start a video - the listener still listens to the song 
// this problem isnt there when broadcasting from pc