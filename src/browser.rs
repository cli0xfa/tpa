use anyhow::{Context, Result};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
pub struct Entry {
    pub path: PathBuf,
    pub directory: bool,
    pub name: String,
}

pub struct Browser {
    pub directory: PathBuf,
    pub entries: Vec<Entry>,
    pub selected: usize,
    pub filter: String,
    pub filtering: bool,
    pub message: Option<String>,
}

fn is_video(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(
            e.to_ascii_lowercase().as_str(),
            "mp4"
                | "mkv"
                | "webm"
                | "mov"
                | "avi"
                | "m4v"
                | "mpg"
                | "mpeg"
                | "ts"
                | "mts"
                | "m2ts"
                | "flv"
                | "wmv"
                | "ogv"
                | "3gp"
                | "vob"
                | "gif"
                | "mxf"
        )
    })
}

impl Browser {
    pub fn new(directory: PathBuf) -> Result<Self> {
        let mut browser = Self {
            directory,
            entries: Vec::new(),
            selected: 0,
            filter: String::new(),
            filtering: false,
            message: None,
        };
        browser.reload()?;
        Ok(browser)
    }
    pub fn reload(&mut self) -> Result<()> {
        let entries = fs::read_dir(&self.directory)
            .with_context(|| format!("无法读取目录 {}", self.directory.display()))?;
        let mut list = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let directory = path.is_dir();
            if directory || is_video(&path) {
                list.push(Entry {
                    path,
                    directory,
                    name: entry.file_name().to_string_lossy().into_owned(),
                });
            }
        }
        list.sort_by(|a, b| {
            b.directory
                .cmp(&a.directory)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        self.entries = list;
        self.selected = 0;
        Ok(())
    }
    pub fn visible(&self) -> Vec<&Entry> {
        let filter = self.filter.to_lowercase();
        self.entries
            .iter()
            .filter(|e| e.name.to_lowercase().contains(&filter))
            .collect()
    }
    pub fn move_selection(&mut self, delta: isize) {
        let count = self.visible().len();
        self.selected = if count == 0 {
            0
        } else {
            (self.selected as isize + delta).rem_euclid(count as isize) as usize
        };
    }
    pub fn enter(&mut self, directory: PathBuf) {
        let previous = self.directory.clone();
        self.directory = directory;
        match self.reload() {
            Ok(()) => {
                self.filter.clear();
                self.filtering = false;
                self.message = None;
            }
            Err(e) => {
                self.directory = previous;
                self.message = Some(format!("{e:#}"));
            }
        }
    }
    pub fn parent(&mut self) {
        if let Some(parent) = self.directory.parent().map(Path::to_owned) {
            self.enter(parent);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn directories_first_case_insensitive_filter() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("z-folder")).unwrap();
        fs::write(temp.path().join("A.MP4"), []).unwrap();
        fs::write(temp.path().join("notes.txt"), []).unwrap();
        let mut browser = Browser::new(temp.path().to_owned()).unwrap();
        assert!(browser.entries[0].directory);
        assert_eq!(browser.entries.len(), 2);
        browser.filter = "a.mp4".to_owned();
        assert_eq!(browser.visible().len(), 1);
        browser.move_selection(-1);
        assert_eq!(browser.selected, 0);
    }
}
