//! Local application preferences, independent of the soundbar settings.

use std::path::{Path, PathBuf};
use windows_reactor::WindowTheme;

pub fn theme_file() -> Option<PathBuf> {
    let root = PathBuf::from(std::env::var_os("LOCALAPPDATA")?);
    root.is_absolute()
        .then(|| root.join("halo-soundbar").join("theme.txt"))
}

pub fn load_theme(path: &Path) -> WindowTheme {
    match std::fs::read_to_string(path).unwrap_or_default().trim() {
        "light" => WindowTheme::Light,
        "dark" => WindowTheme::Dark,
        _ => WindowTheme::System,
    }
}

pub fn save_theme(path: &Path, theme: WindowTheme) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let value = match theme {
        WindowTheme::System => "system\n",
        WindowTheme::Light => "light\n",
        WindowTheme::Dark => "dark\n",
    };
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&temporary, value)?;
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestFile(PathBuf);

    impl TestFile {
        fn new() -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            Self(
                std::env::temp_dir()
                    .join(format!("halo-theme-test-{}-{unique}", std::process::id()))
                    .join("theme.txt"),
            )
        }
    }

    impl Drop for TestFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            let _ = std::fs::remove_dir(self.0.parent().unwrap());
        }
    }

    #[test]
    fn saved_theme_survives_restart_and_can_be_replaced() {
        let file = TestFile::new();
        save_theme(&file.0, WindowTheme::Light).unwrap();
        assert_eq!(load_theme(&file.0), WindowTheme::Light);
        save_theme(&file.0, WindowTheme::Dark).unwrap();
        assert_eq!(load_theme(&file.0), WindowTheme::Dark);
        save_theme(&file.0, WindowTheme::System).unwrap();
        assert_eq!(load_theme(&file.0), WindowTheme::System);
    }

    #[test]
    fn missing_or_invalid_preferences_fall_back_to_system() {
        let file = TestFile::new();
        assert_eq!(load_theme(&file.0), WindowTheme::System);
        std::fs::create_dir_all(file.0.parent().unwrap()).unwrap();
        std::fs::write(&file.0, "invalid-theme").unwrap();
        assert_eq!(load_theme(&file.0), WindowTheme::System);
    }
}
