//! The gpui-kit view layer: an [`AppModel`] entity owning the [`Store`],
//! [`ServerState`] and all transient UI state, rendered to reproduce the
//! Tauri/React frontend feature for feature.

use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::dialog::Dialog;
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::radio::{Radio, RadioGroup};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::theme::ActiveTheme;
use gpui_kit::component::{Disableable, WindowExt};
use gpui_kit::*;
use owlet::{Plugin, RemoteTransport, Scope, ServerConfig};
use tokio::runtime::Runtime;

use crate::form::{self, FormValues, Kind};
use crate::server::{ServerState, ServerStatus};
use crate::store::{Entry, Snapshot, Status, Store};
use crate::updater::{self, UpdateInfo, Updater};

/// Minimum time a transient spinner stays visible, so a fast operation still
/// registers visually. Mirrors the React `MIN_SPIN_MS`.
const MIN_SPIN: Duration = Duration::from_millis(600);

/// Editable text fields of the server dialog, kept as retained entities so
/// their contents survive dialog-layer rebuilds.
struct DialogInputs {
    name: Entity<InputState>,
    command: Entity<InputState>,
    cwd: Entity<InputState>,
    wasm: Entity<InputState>,
    url: Entity<InputState>,
    idle_timeout: Entity<InputState>,
    args: Entity<TextareaState>,
    env: Entity<TextareaState>,
    inherit_env: Entity<TextareaState>,
    dirs: Entity<TextareaState>,
    headers: Entity<TextareaState>,
}

/// Open server dialog: the editing target plus the non-text form state that
/// the radio/select/switch controls mutate directly.
struct ServerForm {
    /// The original name when editing; `None` when adding.
    editing: Option<String>,
    inputs: DialogInputs,
    kind: Kind,
    scope: Scope,
    transport: Option<RemoteTransport>,
    enabled: bool,
    cache_lists: bool,
    error: Option<String>,
}

impl ServerForm {
    fn values(&self, cx: &App) -> FormValues {
        FormValues {
            name: self.inputs.name.read(cx).value().to_string(),
            kind: self.kind,
            enabled: self.enabled,
            command: self.inputs.command.read(cx).value().to_string(),
            args: self.inputs.args.read(cx).value().to_string(),
            env: self.inputs.env.read(cx).value().to_string(),
            cwd: self.inputs.cwd.read(cx).value().to_string(),
            wasm: self.inputs.wasm.read(cx).value().to_string(),
            inherit_env: self.inputs.inherit_env.read(cx).value().to_string(),
            dirs: self.inputs.dirs.read(cx).value().to_string(),
            url: self.inputs.url.read(cx).value().to_string(),
            transport: self.transport,
            headers: self.inputs.headers.read(cx).value().to_string(),
            scope: self.scope,
            idle_timeout: self.inputs.idle_timeout.read(cx).value().to_string(),
            cache_lists: self.cache_lists,
        }
    }
}

/// Top-level application state.
pub struct AppModel {
    store: Store,
    server: Arc<ServerState>,
    /// Background Tokio runtime hosting the embedded owlet server.
    runtime: Arc<Runtime>,
    status: ServerStatus,

    error: Option<String>,
    notice: Option<String>,

    update: Option<UpdateInfo>,
    checking: bool,
    up_to_date: bool,

    saving: bool,
    reloading: bool,
    restarting: bool,
    /// Name of the plugin currently installing, if any.
    installing: Option<String>,

    form: Option<ServerForm>,

    // Task handles kept so dropping/replacing them cancels in-flight work.
    _status_poll: Task<()>,
    _notice_timer: Option<Task<()>>,
    _up_to_date_timer: Option<Task<()>>,
    _install: Option<Task<()>>,
    _restart: Option<Task<()>>,
    _check: Option<Task<()>>,
}

impl AppModel {
    /// Builds the application state, opens the store at `path`, starts the
    /// embedded server, and begins polling its status.
    pub fn new(path: std::path::PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("build tokio runtime"),
        );
        let server = Arc::new(ServerState::new(|_| {}));

        let store = Store::open(path.clone());

        // Start the embedded server inside the Tokio runtime.
        {
            let _guard = runtime.enter();
            server.start(path);
        }

        // Poll the server status on the GPUI thread and mirror it into the
        // entity. The server's own notify callback runs on Tokio threads with
        // no GPUI access, so a light poll is the simplest correct bridge.
        let poll_server = Arc::clone(&server);
        let status_poll = cx.spawn(async move |this, cx| {
            let mut last: Option<ServerStatus> = None;
            loop {
                let current = poll_server.status();
                if last.as_ref() != Some(&current) {
                    last = Some(current.clone());
                    let updated = this.update(cx, |this, cx| {
                        this.status = current;
                        cx.notify();
                    });
                    if updated.is_err() {
                        break;
                    }
                }
                cx.background_executor().timer(Duration::from_millis(200)).await;
            }
        });

        let mut model = Self {
            store,
            server,
            runtime,
            status: ServerStatus::Starting,
            error: None,
            notice: None,
            update: None,
            checking: false,
            up_to_date: false,
            saving: false,
            reloading: false,
            restarting: false,
            installing: None,
            form: None,
            _status_poll: status_poll,
            _notice_timer: None,
            _up_to_date_timer: None,
            _install: None,
            _restart: None,
            _check: None,
        };
        model.start_background_update_check(cx);
        let _ = window;
        model
    }

    fn snapshot(&self) -> Snapshot {
        self.store.snapshot()
    }

    fn locked(&self) -> bool {
        self.store.snapshot().load_error.is_some()
    }

    /// Shows a success notice that clears itself after 3 seconds.
    fn set_notice(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.notice = Some(text.into());
        self.error = None;
        self._notice_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(3)).await;
            let _ = this.update(cx, |this, cx| {
                this.notice = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn set_error(&mut self, err: impl Into<String>, cx: &mut Context<Self>) {
        self.error = Some(err.into());
        self.notice = None;
        cx.notify();
    }

    fn clear_banners(&mut self) {
        self.error = None;
        self.notice = None;
    }

    // --- Store operations -------------------------------------------------

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.store.reload();
        self.clear_banners();
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        match self.store.save() {
            Ok(()) => self.set_notice("Saved.", cx),
            Err(e) => self.set_error(format!("{e:#}"), cx),
        }
    }

    fn set_enabled(&mut self, name: &str, enabled: bool, cx: &mut Context<Self>) {
        match self.store.set_enabled(name, enabled) {
            Ok(()) => self.clear_banners(),
            Err(e) => self.set_error(format!("{e:#}"), cx),
        }
        cx.notify();
    }

    fn delete(&mut self, name: &str, cx: &mut Context<Self>) {
        match self.store.delete(name) {
            Ok(()) => self.clear_banners(),
            Err(e) => self.set_error(format!("{e:#}"), cx),
        }
        cx.notify();
    }

    fn uninstall_plugin(&mut self, name: &str, cx: &mut Context<Self>) {
        match Plugin::find(name) {
            Ok(plugin) => match self.store.uninstall_plugin(plugin) {
                Ok(()) => self.clear_banners(),
                Err(e) => self.set_error(format!("{e:#}"), cx),
            },
            Err(e) => self.set_error(format!("{e:#}"), cx),
        }
        cx.notify();
    }

    fn install_plugin(&mut self, name: String, cx: &mut Context<Self>) {
        if self.installing.is_some() || self.locked() {
            return;
        }
        let plugin = match Plugin::find(&name) {
            Ok(plugin) => plugin,
            Err(e) => {
                self.set_error(format!("{e:#}"), cx);
                return;
            }
        };
        let dir = match self.store.plugins_dir() {
            Ok(dir) => dir,
            Err(e) => {
                self.set_error(format!("{e:#}"), cx);
                return;
            }
        };

        self.installing = Some(name.clone());
        cx.notify();

        // `Plugin::install` downloads the component from the matching GitHub
        // release and verifies its checksum, so it must run async on the Tokio
        // runtime. Keep the spinner visible for at least MIN_SPIN, like the old
        // UI, then add the draft server entry and report the result.
        let runtime = Arc::clone(&self.runtime);
        self._install = Some(cx.spawn(async move |this, cx| {
            let min = cx.background_executor().timer(MIN_SPIN);
            let install = runtime
                .spawn(async move { plugin.install(&dir).await.map(|_| ()) })
                .await;
            min.await;
            let _ = this.update(cx, |this, cx| {
                this.installing = None;
                match install {
                    Ok(Ok(())) => match this.store.install_plugin(plugin) {
                        Ok(()) => {
                            this.set_notice(format!("Installed {name}. Save to add its server."), cx)
                        }
                        Err(e) => this.set_error(format!("{e:#}"), cx),
                    },
                    Ok(Err(e)) => this.set_error(format!("{e:#}"), cx),
                    Err(e) => this.set_error(format!("install task failed: {e}"), cx),
                }
                cx.notify();
            });
        }));
    }

    // --- Server lifecycle -------------------------------------------------

    fn start_server(&mut self, cx: &mut Context<Self>) {
        let path = self.store.path().to_path_buf();
        let server = Arc::clone(&self.server);
        let _guard = self.runtime.enter();
        server.start(path);
        cx.notify();
    }

    fn stop_server(&mut self, cx: &mut Context<Self>) {
        let server = Arc::clone(&self.server);
        self.runtime.spawn(async move { server.stop().await });
        cx.notify();
    }

    fn restart_server(&mut self, cx: &mut Context<Self>) {
        if self.restarting || self.snapshot().dirty {
            return;
        }
        self.restarting = true;
        let server = Arc::clone(&self.server);
        let runtime = Arc::clone(&self.runtime);
        let path = self.store.path().to_path_buf();
        self._restart = Some(cx.spawn(async move |this, cx| {
            let min = cx.background_executor().timer(MIN_SPIN);
            // Stop, then start, then wait for the status to leave Starting.
            let op_server = Arc::clone(&server);
            let op_runtime = Arc::clone(&runtime);
            let op_path = path.clone();
            let op = runtime.spawn(async move {
                op_server.stop().await;
                {
                    let _guard = op_runtime.enter();
                    op_server.start(op_path);
                }
            });
            let _ = op.await;
            for _ in 0..100 {
                if !matches!(server.status(), ServerStatus::Starting) {
                    break;
                }
                cx.background_executor().timer(Duration::from_millis(50)).await;
            }
            min.await;
            let _ = this.update(cx, |this, cx| {
                this.restarting = false;
                cx.notify();
            });
        }));
        cx.notify();
    }

    // --- Updates ----------------------------------------------------------

    fn start_background_update_check(&mut self, cx: &mut Context<Self>) {
        let Ok(Some(updater)) = Updater::for_this_platform() else {
            return;
        };
        let runtime = Arc::clone(&self.runtime);
        self._check = Some(cx.spawn(async move |this, cx| {
            let result = runtime
                .spawn(async move { updater.check(env!("CARGO_PKG_VERSION")).await })
                .await;
            if let Ok(Ok(Some(info))) = result {
                let _ = this.update(cx, |this, cx| {
                    this.update = Some(info);
                    cx.notify();
                });
            }
        }));
    }

    fn on_check_updates(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(info) = self.update.clone() {
            self.open_update_dialog(info, window, cx);
            return;
        }
        if self.checking {
            return;
        }
        let Ok(Some(updater)) = Updater::for_this_platform() else {
            self.set_error("No update channel for this platform", cx);
            return;
        };
        self.checking = true;
        cx.notify();
        let runtime = Arc::clone(&self.runtime);
        self._check = Some(cx.spawn_in(window, async move |this, cx| {
            let result = runtime
                .spawn(async move { updater.check(env!("CARGO_PKG_VERSION")).await })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.checking = false;
                match result {
                    Ok(Ok(Some(info))) => {
                        this.update = Some(info.clone());
                        this.open_update_dialog(info, window, cx);
                    }
                    Ok(Ok(None)) => {
                        this.error = None;
                        this.up_to_date = true;
                        this._up_to_date_timer = Some(cx.spawn(async move |this, cx| {
                            cx.background_executor().timer(Duration::from_secs(3)).await;
                            let _ = this.update(cx, |this, cx| {
                                this.up_to_date = false;
                                cx.notify();
                            });
                        }));
                    }
                    Ok(Err(e)) => this.set_error(format!("Cannot check for updates: {e:#}"), cx),
                    Err(e) => this.set_error(format!("Cannot check for updates: {e}"), cx),
                }
                cx.notify();
            });
        }));
    }

    fn open_update_dialog(&self, info: UpdateInfo, window: &mut Window, cx: &mut Context<Self>) {
        let version = info.version().to_owned();
        let current = info.current().to_owned();
        let notes = info.notes().map(str::to_owned);
        let url = info.download_url().to_owned();
        window.open_alert_dialog(cx, move |alert, _window, _cx| {
            let notes = notes.clone();
            let url = url.clone();
            alert
                .title("Update available")
                .description(format!(
                    "Owlet {version} is available (you have {current}). Download it from your browser; the current app keeps running.{}",
                    notes
                        .as_deref()
                        .map(|n| format!("\n\n{n}"))
                        .unwrap_or_default()
                ))
                .show_cancel(true)
                .ok_text("Download")
                .cancel_text("Later")
                .on_ok(move |_, _, _| {
                    let _ = updater::open_download(&url);
                    true
                })
        });
    }

    // --- Server dialog ----------------------------------------------------

    fn open_add_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.locked() {
            return;
        }
        self.open_server_dialog(None, &FormValues::default(), window, cx);
    }

    fn open_edit_dialog(&mut self, entry: &Entry, window: &mut Window, cx: &mut Context<Self>) {
        if self.locked() {
            return;
        }
        let values = form::to_form(&entry.name, &entry.server);
        self.open_server_dialog(Some(entry.name.clone()), &values, window, cx);
    }

    fn open_server_dialog(
        &mut self,
        editing: Option<String>,
        values: &FormValues,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let single = |text: &str, placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
            let text = text.to_owned();
            let placeholder = placeholder.to_owned();
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(text)
            })
        };
        let multi = |text: &str, placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
            let text = text.to_owned();
            let placeholder = placeholder.to_owned();
            cx.new(|cx| {
                TextareaState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(text)
            })
        };

        let inputs = DialogInputs {
            name: single(&values.name, "", window, cx),
            command: single(&values.command, "uvx", window, cx),
            cwd: single(&values.cwd, "", window, cx),
            wasm: single(&values.wasm, "~/.config/owlet/plugins/aws-mcp.wasm", window, cx),
            url: single(&values.url, "https://example.com/mcp", window, cx),
            idle_timeout: single(&values.idle_timeout, "", window, cx),
            args: multi(&values.args, "mcp-server-fetch", window, cx),
            env: multi(&values.env, "", window, cx),
            inherit_env: multi(&values.inherit_env, "AWS_*", window, cx),
            dirs: multi(&values.dirs, "~/.aws", window, cx),
            headers: multi(&values.headers, "", window, cx),
        };

        self.form = Some(ServerForm {
            editing,
            inputs,
            kind: values.kind,
            scope: values.scope,
            transport: values.transport,
            enabled: values.enabled,
            cache_lists: values.cache_lists,
            error: None,
        });

        let model = cx.entity();
        window.open_dialog(cx, move |dialog, _window, cx| {
            model.update(cx, |this, cx| this.render_server_dialog(dialog, cx))
        });
    }

    fn close_server_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.form = None;
        window.close_dialog(cx);
        cx.notify();
    }

    fn submit_server_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &self.form else {
            return;
        };
        let values = form.values(cx);
        let editing = form.editing.clone();
        match form::from_form(&values) {
            Ok((name, server)) => {
                let old = editing.as_deref();
                match self.store.upsert(old, &name, server) {
                    Ok(()) => {
                        self.clear_banners();
                        self.close_server_dialog(window, cx);
                    }
                    Err(e) => self.set_form_error(format!("{e:#}"), cx),
                }
            }
            Err(e) => self.set_form_error(e.to_string(), cx),
        }
    }

    fn set_form_error(&mut self, err: String, cx: &mut Context<Self>) {
        if let Some(form) = &mut self.form {
            form.error = Some(err);
        }
        cx.notify();
    }

    fn render_server_dialog(&mut self, dialog: Dialog, cx: &mut Context<Self>) -> Dialog {
        let Some(form) = &self.form else {
            return dialog;
        };
        let title = match &form.editing {
            Some(name) => format!("Edit {name}"),
            None => "Add server".to_owned(),
        };
        let kind = form.kind;
        let scope = form.scope;
        let is_command = kind == Kind::Command;

        let field = |label: &str, hint: Option<&str>, control: AnyElement| {
            let mut head = div().flex().gap_1().child(label.to_owned());
            if let Some(hint) = hint {
                head = head.child(div().text_xs().child(hint.to_owned()));
            }
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(head)
                .child(control)
        };

        // Kind radio group.
        let kind_index = match kind {
            Kind::Command => 0,
            Kind::Url => 1,
            Kind::Wasm => 2,
        };
        let kind_radio = RadioGroup::horizontal("kind")
            .children([
                Radio::new("kind-command").label("Local command (stdio)"),
                Radio::new("kind-url").label("Remote URL"),
                Radio::new("kind-wasm").label("WASM plugin"),
            ])
            .selected_index(Some(kind_index))
            .on_change(cx.listener(|this, index: &usize, _window, cx| {
                if let Some(form) = &mut this.form {
                    form.kind = match index {
                        1 => Kind::Url,
                        2 => Kind::Wasm,
                        _ => Kind::Command,
                    };
                }
                cx.notify();
            }));

        // Scope radio group ("project" only valid for command servers).
        let scope_index = match scope {
            Scope::Shared => 0,
            Scope::Project => 1,
            Scope::Session => 2,
        };
        let scope_radio = RadioGroup::horizontal("scope")
            .children([
                Radio::new("scope-shared").label("shared"),
                Radio::new("scope-project")
                    .label("project")
                    .disabled(!is_command),
                Radio::new("scope-session").label("session"),
            ])
            .selected_index(Some(scope_index))
            .on_change(cx.listener(|this, index: &usize, _window, cx| {
                if let Some(form) = &mut this.form {
                    form.scope = match index {
                        1 => Scope::Project,
                        2 => Scope::Session,
                        _ => Scope::Shared,
                    };
                }
                cx.notify();
            }));

        let mut body = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(field(
                "Name",
                None,
                Input::new(&form.inputs.name).into_any_element(),
            ))
            .child(field("Type", None, kind_radio.into_any_element()));

        match kind {
            Kind::Command => {
                body = body
                    .child(field(
                        "Command",
                        None,
                        Input::new(&form.inputs.command).into_any_element(),
                    ))
                    .child(field(
                        "Arguments",
                        Some("one per line"),
                        Textarea::new(&form.inputs.args).into_any_element(),
                    ))
                    .child(field(
                        "Environment",
                        Some("KEY=VALUE per line"),
                        Textarea::new(&form.inputs.env).into_any_element(),
                    ))
                    .child(field(
                        "Working directory",
                        None,
                        Input::new(&form.inputs.cwd).into_any_element(),
                    ));
            }
            Kind::Url => {
                let transport_index = match form.transport {
                    None => 0,
                    Some(RemoteTransport::Http) => 1,
                    Some(RemoteTransport::Sse) => 2,
                };
                let transport_radio = RadioGroup::horizontal("transport")
                    .children([
                        Radio::new("transport-default").label("Streamable HTTP (default)"),
                        Radio::new("transport-http").label("http"),
                        Radio::new("transport-sse").label("sse (legacy)"),
                    ])
                    .selected_index(Some(transport_index))
                    .on_change(cx.listener(|this, index: &usize, _window, cx| {
                        if let Some(form) = &mut this.form {
                            form.transport = match index {
                                1 => Some(RemoteTransport::Http),
                                2 => Some(RemoteTransport::Sse),
                                _ => None,
                            };
                        }
                        cx.notify();
                    }));
                body = body
                    .child(field(
                        "URL",
                        None,
                        Input::new(&form.inputs.url).into_any_element(),
                    ))
                    .child(field("Transport", None, transport_radio.into_any_element()))
                    .child(field(
                        "Headers",
                        Some("Key: Value per line"),
                        Textarea::new(&form.inputs.headers).into_any_element(),
                    ));
            }
            Kind::Wasm => {
                body = body
                    .child(field(
                        "WASM component",
                        None,
                        Input::new(&form.inputs.wasm).into_any_element(),
                    ))
                    .child(field(
                        "Arguments",
                        Some("one per line"),
                        Textarea::new(&form.inputs.args).into_any_element(),
                    ))
                    .child(field(
                        "Environment",
                        Some("KEY=VALUE per line"),
                        Textarea::new(&form.inputs.env).into_any_element(),
                    ))
                    .child(field(
                        "Inherited environment",
                        Some("host variable names, PREFIX* allowed, one per line"),
                        Textarea::new(&form.inputs.inherit_env).into_any_element(),
                    ))
                    .child(field(
                        "Read-only directories",
                        Some("one per line"),
                        Textarea::new(&form.inputs.dirs).into_any_element(),
                    ));
            }
        }

        body = body
            .child(field(
                "Scope",
                None,
                scope_radio.into_any_element(),
            ))
            .child(field(
                "Idle timeout",
                Some("seconds, blank = global"),
                Input::new(&form.inputs.idle_timeout).into_any_element(),
            ))
            .child(
                Switch::new("cache-lists")
                    .label("Cache */list results")
                    .checked(form.cache_lists)
                    .on_change(cx.listener(|this, checked: &bool, _window, cx| {
                        if let Some(form) = &mut this.form {
                            form.cache_lists = *checked;
                        }
                        cx.notify();
                    })),
            )
            .child(
                Switch::new("enabled")
                    .label("Enabled")
                    .checked(form.enabled)
                    .on_change(cx.listener(|this, checked: &bool, _window, cx| {
                        if let Some(form) = &mut this.form {
                            form.enabled = *checked;
                        }
                        cx.notify();
                    })),
            );

        if let Some(err) = &form.error {
            body = body.child(
                Alert::error("server-form-error", err.clone())
                    .banner()
                    .into_any_element(),
            );
        }

        let submit_label = if form.editing.is_some() { "Apply" } else { "Add" };

        let footer = gpui_kit::component::dialog::DialogFooter::new()
            .child(
                Button::new("dialog-cancel")
                    .label("Cancel")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.close_server_dialog(window, cx)
                    })),
            )
            .child(
                Button::new("dialog-submit")
                    .primary()
                    .label(submit_label)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.submit_server_dialog(window, cx)
                    })),
            );

        // Esc/cancel clears the retained form state, then lets the dialog close.
        let model = cx.entity();
        dialog
            .title(title)
            .child(body)
            .footer(footer)
            .on_cancel(move |_, _window, cx| {
                model.update(cx, |this, _cx| this.form = None);
                true
            })
    }
}

impl Render for AppModel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snap = self.snapshot();
        let locked = snap.load_error.is_some();
        let changed = snap
            .entries
            .iter()
            .filter(|e| e.status != Status::Unchanged)
            .count();

        let mut root = div()
            .flex()
            .flex_col()
            .gap_3()
            .p_5()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_header(&snap, locked, changed, cx))
            .child(self.render_service(&snap, cx));

        if let ServerStatus::Failed { error } = &self.status {
            root = root.child(
                Alert::error("service-failed", format!("Service failed.\n{error}")).banner(),
            );
        }

        if let Some(load_error) = &snap.load_error {
            root = root.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        Alert::error(
                            "load-error",
                            format!(
                                "Cannot load config. Editing is disabled until the file is fixed.\n{load_error}"
                            ),
                        )
                        .banner(),
                    )
                    .child(
                        Button::new("retry-load")
                            .label("Retry")
                            .on_click(cx.listener(|this, _, _window, cx| this.reload(cx))),
                    ),
            );
        }

        if let Some(error) = &self.error {
            root = root.child(Alert::error("error-banner", error.clone()).banner());
        } else if let Some(notice) = &self.notice {
            root = root.child(Alert::success("notice-banner", notice.clone()).banner());
        }

        if !locked && snap.entries.is_empty() {
            root = root.child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child("No MCP servers configured yet."),
            );
        }

        root = root.child(self.render_servers(&snap, locked, cx));
        root = root.child(div().text_lg().child("Plugins"));
        root = root.child(self.render_plugins(&snap, locked, cx));
        root
    }
}

impl AppModel {
    fn render_header(
        &self,
        snap: &Snapshot,
        locked: bool,
        changed: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut toolbar = div().flex().items_center().gap_2();

        if snap.dirty {
            toolbar = toolbar.child(
                div()
                    .text_color(cx.theme().accent)
                    .child("Unsaved changes"),
            );
        }

        // Check for updates button with three states.
        let update_btn = if self.up_to_date {
            Button::new("update").label("\u{2713} Up to date").disabled(true)
        } else if self.checking {
            Button::new("update").label("Checking\u{2026}").loading(true).disabled(true)
        } else if let Some(info) = &self.update {
            Button::new("update")
                .label(format!("Update to {}", info.version()))
                .on_click(cx.listener(|this, _, window, cx| this.on_check_updates(window, cx)))
        } else {
            Button::new("update")
                .label("Check for updates")
                .on_click(cx.listener(|this, _, window, cx| this.on_check_updates(window, cx)))
        };
        toolbar = toolbar.child(update_btn);

        toolbar = toolbar.child(
            Button::new("add-server")
                .label("Add server")
                .disabled(locked)
                .on_click(cx.listener(|this, _, window, cx| this.open_add_dialog(window, cx))),
        );

        // Reload / Discard toggle.
        let reload_btn = if snap.dirty {
            Button::new("reload")
                .label("Discard")
                .loading(self.reloading)
                .on_click(cx.listener(|this, _, window, cx| this.confirm_discard(window, cx)))
        } else {
            Button::new("reload")
                .label("Reload")
                .loading(self.reloading)
                .on_click(cx.listener(|this, _, _window, cx| this.reload(cx)))
        };
        toolbar = toolbar.child(reload_btn);

        let save_label = if changed > 0 {
            format!("Save ({changed})")
        } else {
            "Save".to_owned()
        };
        toolbar = toolbar.child(
            Button::new("save")
                .primary()
                .label(save_label)
                .disabled(locked || !snap.dirty || self.saving)
                .on_click(cx.listener(|this, _, _window, cx| this.save(cx))),
        );

        div()
            .flex()
            .flex_row()
            .justify_between()
            .items_center()
            .gap_4()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(div().text_xl().child("Owlet"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(snap.path.clone()),
                    ),
            )
            .child(toolbar)
    }

    fn render_service(&self, snap: &Snapshot, cx: &mut Context<Self>) -> impl IntoElement {
        let (dot_color, text) = match &self.status {
            ServerStatus::Starting => (cx.theme().accent, "Starting\u{2026}".to_owned()),
            ServerStatus::Running { listen } => {
                (cx.theme().success, format!("Listening on http://{listen}"))
            }
            ServerStatus::Stopped => (cx.theme().muted_foreground, "Stopped".to_owned()),
            ServerStatus::Failed { .. } => (cx.theme().danger, "Failed".to_owned()),
        };

        let running = matches!(self.status, ServerStatus::Running { .. });
        let starting = matches!(self.status, ServerStatus::Starting);
        let busy = starting || self.restarting;

        let toggle = if running || starting {
            Button::new("stop")
                .label("Stop")
                .disabled(busy)
                .on_click(cx.listener(|this, _, _window, cx| this.stop_server(cx)))
        } else {
            Button::new("start")
                .label("Start")
                .disabled(busy)
                .on_click(cx.listener(|this, _, _window, cx| this.start_server(cx)))
        };

        let restart_disabled = busy || snap.dirty;
        let restart = Button::new("restart")
            .label("Restart")
            .loading(self.restarting)
            .disabled(restart_disabled)
            .tooltip(if snap.dirty {
                "Save your changes first"
            } else {
                "Apply the saved config"
            })
            .on_click(cx.listener(|this, _, _window, cx| this.restart_server(cx)));

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .p_3()
            .border_1()
            .border_color(cx.theme().border)
            .rounded_md()
            .child(div().size_2().rounded_full().bg(dot_color))
            .child(div().flex_1().child(text))
            .child(div().flex().gap_2().child(toggle).child(restart))
    }

    fn render_servers(
        &self,
        snap: &Snapshot,
        locked: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut list = div().flex().flex_col().gap_2();
        for entry in &snap.entries {
            list = list.child(self.render_server_row(entry, locked, cx));
        }
        list
    }

    fn render_server_row(
        &self,
        entry: &Entry,
        locked: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let name = entry.name.clone();
        let enabled = entry.server.enabled;
        let scope_tag = scope_label(entry.server.scope);
        let summary = summary(&entry.server);

        let mut title = div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child(name.clone()))
            .child(tag(scope_tag.to_owned(), cx.theme().muted_foreground, cx));
        if entry.status != Status::Unchanged {
            let (label, color) = match entry.status {
                Status::Added => ("added", cx.theme().success),
                Status::Modified => ("modified", cx.theme().accent),
                Status::Unchanged => ("", cx.theme().muted_foreground),
            };
            title = title.child(tag(label.to_owned(), color, cx));
        }

        let info = div()
            .flex()
            .flex_col()
            .gap_1()
            .flex_1()
            .child(title)
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(summary),
            );

        let edit_name = name.clone();
        let delete_name = name.clone();
        let toggle_name = name.clone();
        let controls = div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                Button::new(SharedString::from(format!("edit-{name}")))
                    .label("Edit")
                    .disabled(locked)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let snap = this.snapshot();
                        if let Some(e) = snap.entries.iter().find(|e| e.name == edit_name) {
                            this.open_edit_dialog(e, window, cx);
                        }
                    })),
            )
            .child(
                Button::new(SharedString::from(format!("delete-{name}")))
                    .danger()
                    .label("Delete")
                    .disabled(locked)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.confirm_delete(delete_name.clone(), window, cx);
                    })),
            )
            .child(
                Switch::new(SharedString::from(format!("toggle-{name}")))
                    .checked(enabled)
                    .disabled(locked)
                    .on_change(cx.listener(move |this, checked: &bool, _window, cx| {
                        this.set_enabled(&toggle_name, *checked, cx);
                    })),
            );

        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_3()
            .p_3()
            .border_1()
            .border_color(cx.theme().border)
            .rounded_md()
            .child(info)
            .child(controls)
    }

    fn render_plugins(
        &self,
        snap: &Snapshot,
        locked: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut list = div().flex().flex_col().gap_2();
        for plugin in &snap.plugins {
            let name = plugin.name.to_owned();
            let entry = plugin
                .server
                .as_deref()
                .and_then(|srv| snap.entries.iter().find(|e| e.name == srv));

            let mut title = div()
                .flex()
                .items_center()
                .gap_2()
                .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child(name.clone()));
            if plugin.installed {
                title = title.child(tag("installed".to_owned(), cx.theme().muted_foreground, cx));
            }

            let info = div()
                .flex()
                .flex_col()
                .gap_1()
                .flex_1()
                .child(title)
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(plugin.description.to_owned()),
                );

            let controls = if let Some(entry) = entry {
                let uninstall_name = name.clone();
                let toggle_name = entry.name.clone();
                let enabled = entry.server.enabled;
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new(SharedString::from(format!("uninstall-{name}")))
                            .danger()
                            .label("Uninstall")
                            .disabled(locked)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.confirm_uninstall(uninstall_name.clone(), window, cx);
                            })),
                    )
                    .child(
                        Switch::new(SharedString::from(format!("plugin-toggle-{name}")))
                            .checked(enabled)
                            .disabled(locked)
                            .on_change(cx.listener(move |this, checked: &bool, _window, cx| {
                                this.set_enabled(&toggle_name, *checked, cx);
                            })),
                    )
            } else {
                let install_name = name.clone();
                let installing = self.installing.as_deref() == Some(name.as_str());
                div().flex().items_center().gap_2().child(
                    Button::new(SharedString::from(format!("install-{name}")))
                        .label("Install")
                        .loading(installing)
                        .disabled(locked || self.installing.is_some())
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.install_plugin(install_name.clone(), cx);
                        })),
                )
            };

            list = list.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .p_3()
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded_md()
                    .child(info)
                    .child(controls),
            );
        }
        list
    }

    // --- Confirm dialogs --------------------------------------------------

    fn confirm_delete(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let model = cx.entity();
        let title = format!("Delete {name}?");
        window.open_alert_dialog(cx, move |alert, _window, _cx| {
            let model = model.clone();
            let name = name.clone();
            alert
                .title(title.clone())
                .description(
                    "The server is removed from the draft. The config file changes only when you press Save.",
                )
                .show_cancel(true)
                .ok_text("Delete")
                .cancel_text("Cancel")
                .on_ok(move |_, _window, cx| {
                    model.update(cx, |this, cx| this.delete(&name, cx));
                    true
                })
        });
    }

    fn confirm_uninstall(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let model = cx.entity();
        let title = format!("Uninstall {name}?");
        window.open_alert_dialog(cx, move |alert, _window, _cx| {
            let model = model.clone();
            let name = name.clone();
            alert
                .title(title.clone())
                .description(
                    "Its server entry is removed from the draft. The plugin file is deleted when you press Save.",
                )
                .show_cancel(true)
                .ok_text("Uninstall")
                .cancel_text("Cancel")
                .on_ok(move |_, _window, cx| {
                    model.update(cx, |this, cx| this.uninstall_plugin(&name, cx));
                    true
                })
        });
    }

    fn confirm_discard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let model = cx.entity();
        window.open_alert_dialog(cx, move |alert, _window, _cx| {
            let model = model.clone();
            alert
                .title("Discard unsaved changes?")
                .description("The draft is replaced with the contents of the config file.")
                .show_cancel(true)
                .ok_text("Discard")
                .cancel_text("Cancel")
                .on_ok(move |_, _window, cx| {
                    model.update(cx, |this, cx| this.reload(cx));
                    true
                })
        });
    }
}

/// One-line summary of a server, mirroring the React `summary()`.
fn summary(s: &ServerConfig) -> String {
    if let Some(wasm) = &s.wasm {
        let mut parts = vec!["wasm".to_owned(), wasm.to_string_lossy().into_owned()];
        parts.extend(s.args.iter().cloned());
        parts.join(" ")
    } else if let Some(url) = &s.url {
        let transport = match s.transport {
            Some(RemoteTransport::Sse) => "sse",
            _ => "http",
        };
        format!("{transport} \u{b7} {url}")
    } else {
        let mut parts = vec![s.command.clone().unwrap_or_default()];
        parts.extend(s.args.iter().cloned());
        parts.join(" ")
    }
}

fn scope_label(scope: Scope) -> &'static str {
    match scope {
        Scope::Shared => "shared",
        Scope::Project => "project",
        Scope::Session => "session",
    }
}

/// A small rounded tag label.
fn tag(text: String, color: gpui::Hsla, _cx: &mut Context<AppModel>) -> impl IntoElement {
    div()
        .text_xs()
        .px_1p5()
        .border_1()
        .border_color(color)
        .text_color(color)
        .rounded_full()
        .child(text)
}
