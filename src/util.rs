use std::{fs::remove_file, path::Path};

use tracing::error;

pub struct DeleteOnDrop<'a> {
    path: Option<&'a Path>,
}

impl Drop for DeleteOnDrop<'_> {
    fn drop(&mut self) {
        if let Some(path) = self.path {
            if let Err(e) = remove_file(path) {
                error!("could not delete {}: {}", path.display(), e);
            }
        };
    }
}

impl<'a> DeleteOnDrop<'a> {
    pub fn new(path: &'a Path) -> Self {
        Self { path: Some(path) }
    }

    pub fn keep(&mut self) {
        self.path = None;
    }
}
