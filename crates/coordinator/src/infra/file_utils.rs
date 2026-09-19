use log::{error, info};
use std::{fs, path::Path};

pub fn create_folder(root_path: &str) {
    let path = Path::new(root_path);

    if !path.exists() || !path.is_dir() {
        // Create the folder if it doesn't exist
        if let Err(err) = fs::create_dir(path) {
            error!("Folder creating folder: {}", err);
        } else {
            info!("Folder created: {}", root_path);
        }
    } else {
        info!("Folder already exists: {}", root_path);
    }
}
