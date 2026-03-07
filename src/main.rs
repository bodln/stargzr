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
// To the heartbead and broadcast statae add the utc time or some other globally acknowledge time, 
// so when someone tunes in or receives update can add to the playback time the time, it is at the backend currently minus the time this was sent at from the frontend
// this is an effort to decrease the difference in playback for broadcaster and its listeners
// also we must keep in mind that if the calclated adjustment when addedto the playback time exceeds the file size(song duration) we do not add it but send playback as is
// also this should be calculated only when seeking and never when switching songs, because we want the listener to start hearing a new song from 0:00 not ex. 0:00,8 or 0:01,1 
// Also dont forget to remove the time correction on the frontend and have it done exclusively on the backend, maybe even have it on the tune in button,
// so when someone tunes in we compare the time of the tune in vs the arrival of the current broadcast state and then adjust accordingly