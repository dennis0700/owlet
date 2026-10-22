use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};
use owlet::{Config, Plugin, ServerConfig};

/// How a draft entry differs from the config file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Unchanged,
    Added,
    Modified,
}

/// One server as shown in the UI.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub server: ServerConfig,
    pub status: Status,
}

/// One catalog plugin as shown in the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInfo {
    pub name: &'static str,
    pub description: &'static str,
    /// The plugin file exists in the plugins directory.
    pub installed: bool,
    /// Draft server entry running the plugin, if any.
    pub server: Option<String>,
}

/// Everything the UI needs to render.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub path: String,
    /// Set when the config file could not be read; editing is disabled then.
    pub load_error: Option<String>,
    /// True when the draft differs from the config file (including deletions).
    pub dirty: bool,
    pub entries: Vec<Entry>,
    pub plugins: Vec<PluginInfo>,
}

/// In-memory draft of the `[servers]` table, written out only by [`Store::save`].
#[derive(Debug)]
pub struct Store {
    path: PathBuf,
    saved: BTreeMap<String, ServerConfig>,
    draft: BTreeMap<String, ServerConfig>,
    load_error: Option<String>,
    /// Plugins uninstalled in the current draft; Save deletes their files.
    pending_uninstall: BTreeSet<&'static str>,
}

impl Store {
    /// Opens the store; a config that cannot be read is reported through
    /// [`Snapshot::load_error`] instead of failing.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let store = Store::open(PathBuf::from("owlet.toml"));
    /// ```
    pub fn open(path: PathBuf) -> Self {
        let mut store = Self {
            path,
            saved: BTreeMap::new(),
            draft: BTreeMap::new(),
            load_error: None,
            pending_uninstall: BTreeSet::new(),
        };
        store.reload();
        store
    }

    /// Discards the draft and re-reads the config file.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// store.reload();
    /// ```
    pub fn reload(&mut self) {
        self.pending_uninstall.clear();
        match Config::load_servers(&self.path) {
            Ok(servers) => {
                self.saved = servers.clone();
                self.draft = servers;
                self.load_error = None;
            }
            Err(e) => {
                self.saved.clear();
                self.draft.clear();
                self.load_error = Some(format!("{e:#}"));
            }
        }
    }

    /// Current state for rendering.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let snap = store.snapshot();
    /// ```
    pub fn snapshot(&self) -> Snapshot {
        let entries = self
            .draft
            .iter()
            .map(|(name, server)| Entry {
                name: name.clone(),
                server: server.clone(),
                status: match self.saved.get(name) {
                    None => Status::Added,
                    Some(old) if old != server => Status::Modified,
                    Some(_) => Status::Unchanged,
                },
            })
            .collect();
        Snapshot {
            path: self.path.to_string_lossy().into_owned(),
            load_error: self.load_error.clone(),
            dirty: self.draft != self.saved,
            entries,
            plugins: self.plugins(),
        }
    }

    /// Catalog plugins with their install state and draft server entry.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let plugins = store.plugins();
    /// ```
    pub fn plugins(&self) -> Vec<PluginInfo> {
        let dir = Plugin::dir(&self.path).ok();
        Plugin::catalog()
            .iter()
            .map(|p| PluginInfo {
                name: p.name,
                description: p.description,
                installed: dir.as_deref().is_some_and(|d| p.file(d).exists()),
                server: dir
                    .as_deref()
                    .and_then(|d| p.server_name(d, &self.draft))
                    .map(str::to_owned),
            })
            .collect()
    }

    /// Directory plugins are installed into; fails while the config cannot be edited.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let dir = store.plugins_dir()?;
    /// ```
    pub fn plugins_dir(&self) -> anyhow::Result<PathBuf> {
        self.editable()?;
        Plugin::dir(&self.path)
    }

    /// Adds the server entry for an installed plugin to the draft (an entry
    /// already running it is kept).
    ///
    /// # Examples
    ///
    /// ```ignore
    /// store.install_plugin(Plugin::find("aws-mcp")?)?;
    /// ```
    pub fn install_plugin(&mut self, plugin: &Plugin) -> anyhow::Result<()> {
        let dir = self.plugins_dir()?;
        plugin.add_server(&dir, &mut self.draft)
    }

    /// Removes the plugin's server entries from the draft; its file is deleted
    /// by [`Store::save`].
    ///
    /// # Examples
    ///
    /// ```ignore
    /// store.uninstall_plugin(Plugin::find("aws-mcp")?)?;
    /// ```
    pub fn uninstall_plugin(&mut self, plugin: &Plugin) -> anyhow::Result<()> {
        let dir = self.plugins_dir()?;
        if plugin.remove_servers(&dir, &mut self.draft).is_empty() {
            bail!("plugin `{}` has no server entry", plugin.name);
        }
        self.pending_uninstall.insert(plugin.name);
        Ok(())
    }

    /// Path of the config file.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let p = store.path();
    /// ```
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Flips `enabled` in the draft.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// store.set_enabled("fetch", false)?;
    /// ```
    pub fn set_enabled(&mut self, name: &str, enabled: bool) -> anyhow::Result<()> {
        self.editable()?;
        let server = self
            .draft
            .get_mut(name)
            .with_context(|| format!("server `{name}` not found"))?;
        server.enabled = enabled;
        Ok(())
    }

    /// Adds a server (`old_name = None`), replaces one, or renames it when
    /// `old_name` differs from `name`. The definition is validated first.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// store.upsert(None, "fetch", server)?;
    /// store.upsert(Some("fetch"), "web", server)?;
    /// ```
    pub fn upsert(
        &mut self,
        old_name: Option<&str>,
        name: &str,
        server: ServerConfig,
    ) -> anyhow::Result<()> {
        self.editable()?;
        let name = name.trim();
        Config::check_server(name, &server)?;
        match old_name {
            None => {
                if self.draft.contains_key(name) {
                    bail!("server `{name}` already exists");
                }
            }
            Some(old) => {
                if !self.draft.contains_key(old) {
                    bail!("server `{old}` not found");
                }
                if old != name {
                    if self.draft.contains_key(name) {
                        bail!("server `{name}` already exists");
                    }
                    self.draft.remove(old);
                }
            }
        }
        self.draft.insert(name.to_owned(), server);
        Ok(())
    }

    /// Removes a server from the draft.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// store.delete("fetch")?;
    /// ```
    pub fn delete(&mut self, name: &str) -> anyhow::Result<()> {
        self.editable()?;
        self.draft
            .remove(name)
            .map(drop)
            .with_context(|| format!("server `{name}` not found"))
    }

    /// Writes the draft to the config file, keeping comments and other keys,
    /// then deletes the files of plugins uninstalled in this draft that the
    /// saved config no longer runs.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// store.save()?;
    /// ```
    pub fn save(&mut self) -> anyhow::Result<()> {
        self.editable()?;
        Config::persist_servers(&self.path, &self.draft)?;
        self.saved = self.draft.clone();
        let pending = std::mem::take(&mut self.pending_uninstall);
        // The config is already saved; a leftover plugin file is harmless, so
        // cleanup failures are ignored instead of reporting Save as failed.
        if let Ok(dir) = Plugin::dir(&self.path) {
            for plugin in Plugin::catalog() {
                if pending.contains(plugin.name) && plugin.server_name(&dir, &self.saved).is_none()
                {
                    let _ = plugin.uninstall(&dir);
                }
            }
        }
        Ok(())
    }

    fn editable(&self) -> anyhow::Result<()> {
        if let Some(e) = &self.load_error {
            bail!("config could not be loaded, fix the file and reload: {e}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(text: &str) -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, text).expect("write");
        (dir, Store::open(path))
    }

    fn cmd(command: &str) -> ServerConfig {
        Config::parse(&format!("[servers.x]\ncommand = \"{command}\""))
            .expect("parse")
            .servers
            .remove("x")
            .expect("x")
    }

    fn status(s: &Store, name: &str) -> Option<Status> {
        s.snapshot()
            .entries
            .iter()
            .find(|e| e.name == name)
            .map(|e| e.status)
    }

    #[test]
    fn toggle_stays_in_memory_until_saved() {
        let (_dir, mut s) = store("# keep\n[servers.a]\ncommand = \"x\"\n");
        assert!(!s.snapshot().dirty);

        s.set_enabled("a", false).expect("toggle");
        assert!(s.snapshot().dirty);
        assert_eq!(status(&s, "a"), Some(Status::Modified));
        let on_disk = std::fs::read_to_string(s.path()).expect("read");
        assert!(!on_disk.contains("enabled"));

        s.set_enabled("a", true).expect("toggle back");
        assert!(!s.snapshot().dirty);

        s.set_enabled("a", false).expect("toggle");
        s.save().expect("save");
        assert!(!s.snapshot().dirty);
        let on_disk = std::fs::read_to_string(s.path()).expect("read");
        assert!(on_disk.contains("# keep") && on_disk.contains("enabled = false"));
        assert!(set_enabled_missing(&mut s));
    }

    fn set_enabled_missing(s: &mut Store) -> bool {
        s.set_enabled("nope", true).is_err()
    }

    #[test]
    fn add_rename_delete_and_reload() {
        let (_dir, mut s) = store("[servers.a]\ncommand = \"x\"\n[servers.b]\ncommand = \"y\"\n");

        s.upsert(None, "c", cmd("z")).expect("add");
        assert_eq!(status(&s, "c"), Some(Status::Added));
        assert!(s.upsert(None, "c", cmd("z")).is_err());

        s.upsert(Some("a"), "a2", cmd("x")).expect("rename");
        assert!(status(&s, "a").is_none());
        assert_eq!(status(&s, "a2"), Some(Status::Added));
        assert!(s.upsert(Some("a2"), "b", cmd("x")).is_err());
        assert!(s.upsert(Some("ghost"), "g", cmd("x")).is_err());

        s.delete("b").expect("delete");
        assert!(s.delete("b").is_err());
        assert!(s.snapshot().dirty);

        s.reload();
        let snap = s.snapshot();
        assert!(!snap.dirty);
        assert_eq!(snap.entries.len(), 2);
    }

    #[test]
    fn rejects_invalid_definitions() {
        let (_dir, mut s) = store("");
        assert!(s.upsert(None, "", cmd("x")).is_err());
        assert!(s.upsert(None, "a/b", cmd("x")).is_err());
        let mut both = cmd("x");
        both.url = Some("http://h/mcp".into());
        assert!(s.upsert(None, "a", both).is_err());
        assert!(s.snapshot().entries.is_empty());
    }

    #[test]
    fn broken_config_blocks_edits_and_saves() {
        let (_dir, mut s) = store("this is = not [valid");
        assert!(s.snapshot().load_error.is_some());
        assert!(s.upsert(None, "a", cmd("x")).is_err());
        assert!(s.save().is_err());
        let on_disk = std::fs::read_to_string(s.path()).expect("read");
        assert_eq!(on_disk, "this is = not [valid");

        std::fs::write(s.path(), "[servers.a]\ncommand = \"x\"\n").expect("fix");
        s.reload();
        assert!(s.snapshot().load_error.is_none());
        assert_eq!(s.snapshot().entries.len(), 1);
    }

    #[test]
    fn missing_file_is_created_on_save() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sub/config.toml");
        let mut s = Store::open(path.clone());
        assert!(s.snapshot().load_error.is_none());
        s.upsert(None, "a", cmd("x")).expect("add");
        s.save().expect("save");
        assert!(Config::load_servers(&path).expect("load").contains_key("a"));
    }

    fn aws() -> &'static Plugin {
        Plugin::find("aws-mcp").expect("catalog")
    }

    fn fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/aws-mcp.wasm")
    }

    #[test]
    fn installed_plugin_joins_the_draft() {
        let (_dir, mut s) = store("[servers.a]\ncommand = \"x\"\n");
        let info = &s.snapshot().plugins[0];
        assert_eq!(
            (info.name, info.installed, info.server.as_deref()),
            ("aws-mcp", false, None)
        );

        let dir = s.plugins_dir().expect("dir");
        aws().install_file(&dir, &fixture()).expect("install");
        s.install_plugin(aws()).expect("add to draft");
        let snap = s.snapshot();
        assert!(snap.dirty);
        assert_eq!(status(&s, "aws-mcp"), Some(Status::Added));
        assert!(snap.plugins[0].installed);
        assert_eq!(snap.plugins[0].server.as_deref(), Some("aws-mcp"));

        // Installing again keeps the (edited) entry.
        s.set_enabled("aws-mcp", false).expect("toggle");
        s.install_plugin(aws()).expect("again");
        assert!(
            !s.snapshot()
                .entries
                .iter()
                .any(|e| e.name == "aws-mcp" && e.server.enabled)
        );

        s.save().expect("save");
        let saved = Config::load_servers(s.path()).expect("load");
        assert_eq!(saved["aws-mcp"].wasm, Some(aws().file(&dir)));
        assert!(aws().file(&dir).exists());
    }

    #[test]
    fn uninstall_deletes_the_file_only_on_save() {
        let (_dir, mut s) = store("");
        let dir = s.plugins_dir().expect("dir");
        aws().install_file(&dir, &fixture()).expect("install");
        s.install_plugin(aws()).expect("add");
        s.save().expect("save");

        s.uninstall_plugin(aws()).expect("uninstall");
        assert!(s.uninstall_plugin(aws()).is_err());
        assert_eq!(status(&s, "aws-mcp"), None);
        assert!(aws().file(&dir).exists(), "file stays until Save");

        s.reload();
        assert_eq!(status(&s, "aws-mcp"), Some(Status::Unchanged));
        s.uninstall_plugin(aws()).expect("uninstall");
        s.save().expect("save");
        assert!(!aws().file(&dir).exists());
        assert!(!s.snapshot().plugins[0].installed);
        assert!(Config::load_servers(s.path()).expect("load").is_empty());
    }

    #[test]
    fn save_keeps_plugin_files_not_uninstalled_in_the_draft() {
        let (_dir, mut s) = store("");
        let dir = s.plugins_dir().expect("dir");
        aws().install_file(&dir, &fixture()).expect("install");

        // No entry runs the file: an unrelated Save keeps it.
        s.upsert(None, "a", cmd("x")).expect("add");
        s.save().expect("save");
        assert!(aws().file(&dir).exists());

        // An entry using a non-canonical path to the file: Save keeps it.
        let mut odd = aws().server_config(&dir);
        odd.wasm = Some(dir.join(".").join("aws-mcp.wasm"));
        s.upsert(None, "odd", odd).expect("add odd");
        s.save().expect("save");
        assert!(aws().file(&dir).exists());

        // Uninstall then Reload discards the pending deletion.
        s.install_plugin(aws()).expect("add");
        s.save().expect("save");
        s.uninstall_plugin(aws()).expect("uninstall");
        s.reload();
        s.save().expect("save");
        assert!(aws().file(&dir).exists());
    }
}
