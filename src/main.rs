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
// also when first opening the site it shows both players even tho the first thing we see is an audio
// it show both the audio and video player 


// TODO
// also on both pc and mobile if im on audio its fine i click next to a video, ok, click next to an audio, it plays, but there is no player controls or anything (this is on private mode)
// this happens only if i was tuned in tho and then went to private 
// then there is no longer a player for audio tho sound i played


// TODO
// say we start off (the first thing we play) with playing a video element on index 3 and indexes 4 and 5 are audio, if i click next it does play those but it doesnt send the broadcast update that it started plaing those
// this only happens while the media type dffers from the first one played, if index 6 is a video it sends the update
// to reiterate, if i start a video media -> click start broadcasting -> works and people can tune into the video -> broadcaster plays an audio -> aduio plays for broadcaster -> but no braodcast update is sent -> if now broadcaster plays a video -> it starts a video for broadcaster -> but now successfully sends broadcast update for the cchange -> clients too now see that new video change
// this works the same way but in reverse if we start from streaming audio