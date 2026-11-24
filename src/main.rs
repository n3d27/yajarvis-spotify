use jarvis_plugin_sdk::process;
use spotify_core::plugin::SpotifyPlugin;

fn start(init: process::Init) -> Result<Box<dyn jarvis_plugin_sdk::Plugin>, String> {
    Ok(Box::new(SpotifyPlugin::new(&init.settings)))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    process::serve(start).await
}
