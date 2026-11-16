//! A [`ContactSource`] that reads a folder of `.vcf` files.
//!
//! This is the "vdir" layout that vdirsyncer and khard use, and what iccontacts
//! writes: one address book per subfolder, one or more cards per file. It lets
//! the app show contact names on desktops without Evolution Data Server (KDE,
//! most tiling setups) as long as something syncs vCards into the folder.

use std::path::{Path, PathBuf};

use anyhow::bail;
use async_trait::async_trait;

use crate::contacts::ContactSource;
use crate::store::Contact;
use crate::vcard::parse_vcards;

/// How deep to look below the root. vdirs are `root/<book>/<card>.vcf`; a
/// little slack allows one more level of grouping without following a
/// symlink loop forever.
const MAX_DEPTH: usize = 4;

pub struct VdirContactSource {
    root: PathBuf,
}

impl VdirContactSource {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `$XDG_DATA_HOME/contacts` (usually `~/.local/share/contacts`). Inside a
    /// Flatpak, GLib's data dir is the sandbox's own, so use the host path the
    /// manifest exposes read-only (`--filesystem=xdg-data/contacts:ro`).
    pub fn default_root() -> PathBuf {
        if Path::new("/.flatpak-info").exists() {
            glib::home_dir().join(".local/share/contacts")
        } else {
            glib::user_data_dir().join("contacts")
        }
    }
}

#[async_trait]
impl ContactSource for VdirContactSource {
    async fn fetch_all(&self) -> anyhow::Result<Vec<Contact>> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || read_all(&root)).await?
    }
}

fn read_all(root: &Path) -> anyhow::Result<Vec<Contact>> {
    if !root.is_dir() {
        bail!("no vCard folder at {}", root.display());
    }
    let mut files = Vec::new();
    collect_vcf_files(root, 0, &mut files);
    files.sort();

    let mut contacts = Vec::new();
    for file in files {
        match std::fs::read_to_string(&file) {
            Ok(text) => contacts.extend(parse_vcards(&text)),
            Err(e) => eprintln!("skipping unreadable vCard {}: {e}", file.display()),
        }
    }
    Ok(contacts)
}

/// Collect `*.vcf` files under `dir`. Hidden entries are skipped: sync tools
/// write `.name.tmp` files before renaming them into place.
fn collect_vcf_files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let hidden = entry.file_name().to_string_lossy().starts_with('.');
        if hidden {
            continue;
        }
        if path.is_dir() {
            if depth < MAX_DEPTH {
                collect_vcf_files(&path, depth + 1, out);
            }
        } else if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("vcf"))
        {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn card(uid: &str, name: &str) -> String {
        format!("BEGIN:VCARD\nVERSION:3.0\nUID:{uid}\nFN:{name}\nTEL:+1555{uid}\nEND:VCARD\n")
    }

    fn names(contacts: &[Contact]) -> Vec<&str> {
        contacts.iter().map(|c| c.display_name.as_str()).collect()
    }

    #[tokio::test]
    async fn reads_cards_from_every_address_book() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("icloud")).unwrap();
        fs::create_dir_all(dir.path().join("work")).unwrap();
        fs::write(dir.path().join("icloud/a.vcf"), card("1", "Ann")).unwrap();
        fs::write(dir.path().join("icloud/b.VCF"), card("2", "Bob")).unwrap();
        fs::write(dir.path().join("work/c.vcf"), card("3", "Cat") + &card("4", "Dan")).unwrap();

        let contacts = VdirContactSource::new(dir.path()).fetch_all().await.unwrap();
        assert_eq!(names(&contacts), ["Ann", "Bob", "Cat", "Dan"]);
    }

    #[tokio::test]
    async fn ignores_hidden_and_non_vcard_files() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("keep.vcf"), card("1", "Kept")).unwrap();
        fs::write(dir.path().join(".partial.vcf"), card("2", "Temp")).unwrap();
        fs::write(dir.path().join("notes.txt"), card("3", "Text")).unwrap();
        fs::create_dir_all(dir.path().join(".cache")).unwrap();
        fs::write(dir.path().join(".cache/x.vcf"), card("4", "Cached")).unwrap();

        let contacts = VdirContactSource::new(dir.path()).fetch_all().await.unwrap();
        assert_eq!(names(&contacts), ["Kept"]);
    }

    #[tokio::test]
    async fn missing_folder_is_an_error_so_the_cache_is_kept() {
        let dir = TempDir::new().unwrap();
        let result = VdirContactSource::new(dir.path().join("absent")).fetch_all().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn empty_folder_is_ok_and_empty() {
        let dir = TempDir::new().unwrap();
        let contacts = VdirContactSource::new(dir.path()).fetch_all().await.unwrap();
        assert!(contacts.is_empty());
    }
}
