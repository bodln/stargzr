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

// TODO
// on get session id make sre the old session is still valid in the session, meaning it wasnt cleaned up, maybe add an endpoint for checking the session and reloading on the frontend
// then remove the get session id function in session.rs and this just does it for you...idk, check that on song play too maybe and only send alert that you need to refresh
// for now i will set the session cleanup to be way less frequent




// TODO
// also rate limiting acted up once, i stopped broadcast, might be something with heartbeats. must look into it more

// TODO
// maybe figure out a way to resolve the cutting off of the last few seconds of the song for the tunedin listener when he is that many seconds behind the broadcaster, 
// and the broadcaster goes to the next song automatically when he completely finishes the song he is broadcasting


// maybe the broadcaster should wait a second before autoomatically goingto the next song when the song he is listening to reaches the end, just to give time for the listeners to finish


// or implement some special thing that gets sent when broadcaster does auto next meaning that the listeners should first finish their own file and then tune in to the broadcaster
// (tho that doesnt solve missing song part, because then the listner will either be even more behind

// (which might be feasible, in a way that the listeners are always behind and rmeember the broadcast update for the auto next song signal and jumps to that song at the beginning of it when its own song is finished, 
// and the only time we actually follow the braodcast leader is when he seeks/pauses/ jumps to another song manually),

// or would need to jump over the beginning of the next song to be exactly in sync with the broadcaster)







// TODO
// To the heartbead and broadcast statae add the utc time or some other globally acknowledge time, 
// so when someone tunes in or receives update can add to the playback time the time, it is at the backend currently minus the time this was sent at from the frontend
// this is an effort to decrease the difference in playback for broadcaster and its listeners
// also we must keep in mind that if the calclated adjustment when addedto the playback time exceeds the file size(song duration) we do not add it but send playback as is
// also this should be calculated only when seeking and never when switching songs, because we want the listener to start hearing a new song from 0:00 not ex. 0:00,8 or 0:01,1 