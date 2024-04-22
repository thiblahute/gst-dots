use dirs::cache_dir;
use glob::glob;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Determine the directory to use for dumping GStreamer pipelines
    let gstdot_path = env::var("GST_DEBUG_DUMP_DOT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let mut path = cache_dir().expect("Failed to find cache directory");
            path.push("gstreamer-dots");
            path
        });

    // Ensure the directory exists
    fs::create_dir_all(&gstdot_path).expect("Failed to create dot directory");

    println!("Dumping GStreamer pipelines into {:?}", gstdot_path);

    // Build the glob pattern and remove existing .dot files
    let pattern = gstdot_path.join("*.dot*").to_string_lossy().into_owned();
    for entry in glob(&pattern).expect("Failed to read glob pattern") {
        match entry {
            Ok(path) => {
                if path.is_file() {
                    fs::remove_file(path).expect("Failed to remove file");
                }
            }
            Err(e) => eprintln!("Error reading file: {}", e),
        }
    }

    // Set the environment variable to use the determined directory
    env::set_var("GST_DEBUG_DUMP_DOT_DIR", &gstdot_path);

    // Run the command provided in arguments
    let args: Vec<String> = env::args().skip(1).collect();
    eprintln!("Running {args:?}");
    if !args.is_empty() {
        let output = Command::new(&args[0]).args(&args[1..]).status();

        match output {
            Ok(_status) => (),
            Err(e) => eprintln!("Error: {e:?}"),
        }
    }
}
