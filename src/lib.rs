//! OpenIDEA shell for the Apifox export.
//!
//! Two surfaces share one storage:
//! - the `settings` view holds the server + access token;
//! - the `mapping` view holds one row per **code module** (Maven/Gradle),
//!   each mapping to its own Apifox project / branch / module.
//!
//! Double-clicking a row opens a form with real dropdowns (branch/module),
//! because table cells are text-only in this host. `Apifox Sync: 同步` walks the
//! enabled rows, scans the module's sources and imports each separately.

use apifox_core::{
    apifox, openapi, postman, prefix_paths, project as code_project, scan_modules_scoped,
    yaml,
};
use openidea_plugin::views::{
    Body, Button, Field, FieldValue, Table, View, ViewEvent, Viewport, Views,
};
use openidea_plugin::{
    host, register_plugin, Error, HttpRequest, Invocation, LogLevel, NotificationLevel, Plugin,
    Project,
};
use serde_json::{json, Value};

const TITLE: &str = "Apifox Sync";
const MAX_FILES: u32 = 20_000;
const OPENAPI_PATH: &str = "docs/openapi.json";
const APIFOX_SECRET: &str = "apifox_token";

/// One code module -> one Apifox target.
#[derive(Clone)]
struct Mapping {
    enabled: bool,
    /// Display name (Maven artifactId / Gradle project name).
    name: String,
    /// Directory relative to the project root; `""` = whole project.
    path: String,
    project_id: String,
    /// Apifox team/project display names, kept so the edit form can restore
    /// the dropdown selection (the API only exposes names, not ids).
    team_id: Option<u64>,
    team_name: String,
    project_name: String,
    branch_id: Option<u64>,
    branch_name: String,
    module_id: Option<u64>,
    module_name: String,
    /// Apifox folder ids; `None` = project root.
    endpoint_folder_id: Option<u64>,
    schema_folder_id: Option<u64>,
    /// Overwrite behaviours (`AUTO_MERGE`, `OVERWRITE_EXISTING`, ...).
    endpoint_mode: String,
    schema_mode: String,
    /// Property/parameter naming (`original`, `camel`, `snake`, ...).
    naming: String,
    /// `updateFolderOfChangedEndpoint`.
    update_folder: bool,
}

impl Default for Mapping {
    fn default() -> Self {
        Self {
            enabled: true,
            name: String::new(),
            path: String::new(),
            project_id: String::new(),
            team_id: None,
            team_name: String::new(),
            project_name: String::new(),
            branch_id: None,
            branch_name: String::new(),
            module_id: None,
            module_name: String::new(),
            endpoint_folder_id: None,
            schema_folder_id: None,
            endpoint_mode: "AUTO_MERGE".into(),
            schema_mode: "AUTO_MERGE".into(),
            naming: "original".into(),
            update_folder: false,
        }
    }
}

impl Mapping {
    fn branch_label(&self) -> String {
        if self.branch_name.is_empty() {
            "(主分支)".to_string()
        } else {
            self.branch_name.clone()
        }
    }

    fn module_label(&self) -> String {
        if self.module_name.is_empty() {
            "(默认模块)".to_string()
        } else {
            self.module_name.clone()
        }
    }
}

/// One module's upload, kept after the toast so the `report` view can list it
/// (the reference plugin prints this per module into its console).
#[derive(Clone, Default)]
struct ImportEntry {
    module: String,
    path: String,
    project_id: String,
    project_name: String,
    branch: String,
    apifox_module: String,
    endpoint_folder: String,
    schema_folder: String,
    endpoints: usize,
    /// `GET /report/summary — 摘要`, capped for the view.
    endpoint_list: Vec<String>,
    outcome: Option<apifox::ImportOutcome>,
    /// Why the module was skipped or failed (shown instead of counters).
    note: Option<String>,
}

/// Everything one sync produced: per-module results plus the totals table.
#[derive(Clone, Default)]
struct ImportReport {
    entries: Vec<ImportEntry>,
    created: u64,
    updated: u64,
    ignored: u64,
    failed: u64,
    schemas_created: u64,
    schemas_updated: u64,
    /// `扫描 812 个 Java 文件，5 个模块里的 96 个` — what the scan covered.
    scanned: String,
}

#[derive(Default)]
pub struct Main {
    mappings: Vec<Mapping>,
    /// Root the mappings were loaded for.
    loaded_for: String,
    /// Row being edited in the form (None = table).
    editing: Option<usize>,
    e_enabled: bool,
    e_project: String,
    e_branch: String,
    e_module: String,
    e_endpoint_folder: String,
    e_schema_folder: String,
    e_endpoint_mode: String,
    e_schema_mode: String,
    e_naming: String,
    e_update_folder: bool,
    server: String,
    server_draft: String,
    teams: Vec<apifox::Named>,
    projects: Vec<apifox::Named>,
    detected: Vec<code_project::CodeModule>,
    e_team: String,
    e_team_id: Option<u64>,
    e_project_pick: String,
    e_code_module: String,
    endpoint_folders: Vec<apifox::Folder>,
    schema_folders: Vec<apifox::Folder>,
    selected_row: Option<usize>,
    branches: Vec<apifox::Branch>,
    modules: Vec<apifox::Module>,
    token_draft: String,
    status: Option<String>,
    /// Last sync's result, rendered by the `report` view.
    report: Option<ImportReport>,
}

impl Plugin for Main {
    fn run_command(
        &mut self,
        id: &str,
        ctx: &Invocation,
        _arg: Option<&str>,
    ) -> Result<Option<String>, Error> {
        match id {
            "sync-apifox" => self.sync(ctx),
            "export-openapi" => export(ctx),
            "export-yaml" => export_yaml(ctx),
            "export-postman" => export_postman(ctx),
            "configure-apifox" => configure(ctx),
            other => Err(format!("未知命令 {other}").into()),
        }
    }
}

const NO_BRANCH: &str = "(主分支)";
const DEFAULT_MODULE: &str = "(默认模块)";

impl Views for Main {
    fn render(
        &mut self,
        view: &str,
        _viewport: Option<Viewport>,
        ctx: &Invocation,
    ) -> Result<View, Error> {
        let Some(project) = ctx.project() else {
            return Ok(View::message("请先打开一个 Java 项目，再打开 Apifox Sync。"));
        };
        let root = project.root().unwrap_or_default();
        if self.loaded_for != root {
            self.mappings = load_mappings(&root);
            self.server = server_base(&root);
            self.loaded_for = root.clone();
            self.editing = None;
        }
        if view == "report" {
            return Ok(self.report_view());
        }
        if view == "mapping" {
            if let Some(i) = self.editing {
                if i < self.mappings.len() {
                    return Ok(self.editor_view(i, &root));
                }
                self.editing = None;
            }
            return Ok(self.mapping_view(&root));
        }
        Ok(self.settings_view())
    }

    fn handle(&mut self, view: &str, event: ViewEvent, ctx: &Invocation) -> Result<(), Error> {
        let root = ctx
            .project()
            .map(|p| p.root().unwrap_or_default())
            .unwrap_or_default();

        if view == "report" {
            if let ViewEvent::Button(id) = event {
                match id.as_str() {
                    "clear_report" => {
                        self.report = None;
                        host::refresh_view(view);
                    }
                    "back_to_mapping" => {
                        let _ = host::reveal_view("mapping");
                    }
                    _ => {}
                }
            }
            return Ok(());
        }
        if view == "mapping" {
            if let Some(i) = self.editing {
                self.handle_editor(i, event, &root, ctx)?;
            } else {
                self.handle_table(event, &root, ctx)?;
            }
        } else {
            match event {
                ViewEvent::Field(key, value) if key == "token" => {
                    self.token_draft = value;
                    return Ok(());
                }
                ViewEvent::Field(key, value) if key == "server_url" => {
                    self.server_draft = value.trim().to_string();
                    return Ok(());
                }
                ViewEvent::Field(key, value) if key == "server_kind" => {
                    if value == "公网 SaaS" {
                        self.server_draft = DEFAULT_SERVER.to_string();
                    }
                    return Ok(());
                }
                ViewEvent::Button(id) if id == "save_server" => {
                    let url = if self.server_draft.trim().is_empty() {
                        DEFAULT_SERVER.to_string()
                    } else {
                        self.server_draft.trim().trim_end_matches('/').to_string()
                    };
                    if !url.starts_with("http://") && !url.starts_with("https://") {
                        self.status = Some("服务器地址必须以 http:// 或 https:// 开头".into());
                        return Ok(());
                    }
                    host::storage::set(&server_key(&root), &url)?;
                    self.server = url.clone();
                    self.server_draft.clear();
                    self.status = Some(format!("服务器地址已设为 {url}"));
                }
                ViewEvent::Button(id) if id == "test" => {
                    let project = self
                        .mappings
                        .iter()
                        .find_map(|m| (!m.project_id.is_empty()).then(|| m.project_id.clone()));
                    match project {
                        Some(pid) => {
                            self.status = Some(match call_apifox(&apifox::branches_request(&pid), &self.server) {
                                Ok((status, _)) if (200..300).contains(&status) => {
                                    format!("连接正常（项目 {pid} 的分支列表可读）")
                                }
                                Ok((status, text)) => format!("连接失败：HTTP {status} {text}"),
                                Err(e) => format!("连接失败：{e}"),
                            });
                        }
                        None => self.status = Some("先在映射里填一个 Apifox 项目 ID".into()),
                    }
                }
                ViewEvent::Button(id) if id == "save_token" => {
                    if !self.token_draft.trim().is_empty() {
                        let raw = self.token_draft.trim();
                        let token = raw.strip_prefix("Bearer ").unwrap_or(raw).trim();
                        host::secret_set(APIFOX_SECRET, token)?;
                        self.token_draft.clear();
                        self.status = Some("访问令牌已保存到钥匙串".into());
                    }
                }
                _ => return Ok(()),
            }
        }
        host::refresh_view(view);
        Ok(())
    }
}

impl Main {
    fn settings_view(&mut self) -> View {
        let token_set = host::secret_is_set(APIFOX_SECRET);
        let public = self.server == DEFAULT_SERVER;
        let fields = vec![
            Field::new(
                "server_kind",
                "Apifox 服务器",
                FieldValue::Choice(
                    vec!["公网 SaaS".into(), "私有化部署".into()],
                    if public { "公网 SaaS".into() } else { "私有化部署".into() },
                ),
            ),
            Field::new(
                "server_url",
                "地址",
                FieldValue::Text(if self.server_draft.is_empty() {
                    self.server.clone()
                } else {
                    self.server_draft.clone()
                }),
            ),
            Field::new(
                "token",
                "API 访问令牌（留空保持不变）",
                FieldValue::Text(self.token_draft.clone()),
            ),
            Field::new(
                "token_state",
                "令牌状态",
                FieldValue::label(if token_set { "已保存到系统钥匙串" } else { "未设置" }),
            ),
            Field::new(
                "hint",
                "映射",
                FieldValue::label("在「映射」标签里按代码模块配置项目/分支/模块"),
            ),
        ];
        View::new(Body::Form(fields))
            .button(Button::new("save_server", "保存服务器"))
            .button(Button::new("save_token", "保存令牌"))
            .button(Button::new("test", "测试连接"))
            .status(
                &self
                    .status
                    .clone()
                    .unwrap_or_else(|| "令牌存钥匙串，插件读不到明文".into()),
            )
    }

    /// The upload report the reference plugin prints into its console: per
    /// module the Apifox target, the counters, then the totals table.
    fn report_view(&self) -> View {
        let Some(report) = &self.report else {
            return View::new(Body::Message(
                "还没有导入记录：在「映射」页点「同步全部」，这里会按模块列出导入结果。".into(),
            ));
        };
        let mut html = String::new();
        html.push_str(
            "<div style=\"font-family:var(--openidea-font-mono,ui-monospace,Consolas,monospace);font-size:12px;line-height:1.65;
             white-space:pre-wrap;padding:4px 6px\">",
        );
        html.push_str(&format!(
            "<div style=\"font-size:13px;font-weight:600\">导入结果</div>\
             <div style=\"color:var(--openidea-fg-muted)\">共 {} 个模块：接口新建 {}、更新 {}、忽略 {}、失败 {}；\
             数据模型新建 {}、更新 {}</div>",
            report.entries.len(),
            report.created,
            report.updated,
            report.ignored,
            report.failed,
            report.schemas_created,
            report.schemas_updated
        ));
        if !report.scanned.is_empty() {
            html.push_str(&format!(
                "<div style=\"color:var(--openidea-fg-muted)\">{}</div>",
                esc(&report.scanned)
            ));
        }

        for entry in &report.entries {
            html.push_str(
                "<div style=\"margin-top:12px;font-weight:600;border-top:1px solid var(--openidea-border);\
                 padding-top:8px\">",
            );
            html.push_str(&format!("▌ {}", esc(&entry.module)));
            if !entry.path.is_empty() {
                html.push_str(&format!(
                    " <span style=\"color:var(--openidea-fg-muted)\">({})</span>",
                    esc(&entry.path)
                ));
            }
            html.push_str("</div>");
            html.push_str(&format!(
                "<div style=\"color:var(--openidea-fg)\">项目ID: {}，项目名称: {}，分支/版本: {}，模块: {}，\
                 接口路径: {}，数据模型路径: {}。{}</div>",
                esc(&entry.project_id),
                esc(&entry.project_name),
                esc(&entry.branch),
                esc(&entry.apifox_module),
                esc(&entry.endpoint_folder),
                esc(&entry.schema_folder),
                if entry.outcome.is_some() {
                    "导入成功"
                } else {
                    "未导入"
                }
            ));
            match (&entry.outcome, &entry.note) {
                (Some(out), _) => {
                    html.push_str(&format!(
                        "<div style=\"color:var(--openidea-fg-muted)\">接口 {} 个：新建 {}、更新 {}、忽略 {}、失败 {}｜\
                         数据模型：新建 {}、更新 {}</div>",
                        entry.endpoints,
                        out.created,
                        out.updated,
                        out.ignored,
                        out.failed,
                        out.schemas_created,
                        out.schemas_updated
                    ));
                    if !out.errors.is_empty() {
                        html.push_str(&format!(
                            "<div style=\"font-weight:600\">⚠ {}</div>",
                            esc(&out.errors.join("；"))
                        ));
                    }
                }
                (None, note) => {
                    html.push_str(&format!(
                        "<div style=\"font-weight:600\">⚠ {}</div>",
                        esc(note.as_deref().unwrap_or("跳过了这个模块"))
                    ));
                }
            }
            if entry.outcome.is_some() && !entry.endpoint_list.is_empty() {
                html.push_str("<div style=\"color:var(--openidea-fg-muted)\">");
                for line in &entry.endpoint_list {
                    html.push_str(&format!("  {}<br>", esc(line)));
                }
                let rest = entry.endpoints.saturating_sub(entry.endpoint_list.len());
                if rest > 0 {
                    html.push_str(&format!("  … 还有 {rest} 个接口"));
                }
                html.push_str("</div>");
            }
        }

        html.push_str(
            "<table style=\"border-collapse:collapse;margin-top:14px;font-size:12px\">\
             <tr><th style=\"text-align:left;padding:3px 18px 3px 0;border-bottom:1px solid var(--openidea-border)\">Type</th>\
             <th style=\"text-align:right;padding:3px 18px 3px 0;border-bottom:1px solid var(--openidea-border)\">Created</th>\
             <th style=\"text-align:right;padding:3px 18px 3px 0;border-bottom:1px solid var(--openidea-border)\">Updated</th>\
             <th style=\"text-align:right;padding:3px 18px 3px 0;border-bottom:1px solid var(--openidea-border)\">Ignored</th>\
             <th style=\"text-align:right;padding:3px 0;border-bottom:1px solid var(--openidea-border)\">Failed</th></tr>",
        );
        let row = |name: &str, created: u64, updated: u64, ignored: Option<u64>, failed: u64| {
            format!(
                "<tr><td style=\"padding:3px 18px 3px 0\">{name}</td>\
                 <td style=\"text-align:right;padding:3px 18px 3px 0\">{created}</td>\
                 <td style=\"text-align:right;padding:3px 18px 3px 0\">{updated}</td>\
                 <td style=\"text-align:right;padding:3px 18px 3px 0\">{}</td>\
                 <td style=\"text-align:right;padding:3px 0\">{failed}</td></tr>",
                ignored.map(|v| v.to_string()).unwrap_or_else(|| "-".into())
            )
        };
        html.push_str(&row(
            "Endpoints",
            report.created,
            report.updated,
            Some(report.ignored),
            report.failed,
        ));
        // Apifox reports no "ignored" counter for schemas; `-` says so.
        html.push_str(&row(
            "Schemas",
            report.schemas_created,
            report.schemas_updated,
            None,
            0,
        ));
        html.push_str("</table></div>");

        View::new(Body::Html(html))
            .button(Button::new("back_to_mapping", "回映射"))
            .button(Button::new("clear_report", "清空"))
            .status(&format!(
                "上次同步 {} 个模块：接口新建 {}、更新 {}、失败 {}",
                report.entries.len(),
                report.created,
                report.updated,
                report.failed
            ))
    }

    fn mapping_view(&self, root: &str) -> View {
        let rows: Vec<Vec<String>> = self
            .mappings
            .iter()
            .map(|m| {
                vec![
                    if m.enabled { "是".into() } else { "否".into() },
                    if m.name.is_empty() { m.path.clone() } else { m.name.clone() },
                    if m.project_id.is_empty() { "—".into() } else { m.project_id.clone() },
                    m.branch_label(),
                    m.module_label(),
                    folder_label(m.endpoint_folder_id),
                    folder_label(m.schema_folder_id),
                    format!("{} / {}", mode_label(&m.endpoint_mode), mode_label(&m.schema_mode)),
                ]
            })
            .collect();
        let table = Table {
            columns: vec![
                "启用".into(),
                "代码模块".into(),
                "Apifox 项目".into(),
                "分支/版本".into(),
                "模块".into(),
                "接口目录".into(),
                "模型目录".into(),
                "覆盖策略（接口/模型）".into(),
            ],
            total_rows: rows.len() as u32,
            first_row: 0,
            rows,
            // Not editable: double-click/Enter must open the editor form, where
            // the project ID, branch and module are; editable cells swallow it.
            editable: false,
        };
        let status = self.status.clone().unwrap_or_else(|| {
            if self.mappings.is_empty() {
                "点「新增」新建一行，再在编辑页选源代码模块".into()
            } else {
                "双击任意行打开编辑（项目 ID、分支、模块、启用都在里面）".to_string()
            }
        });
        let _ = root;
        View::new(Body::Table(table))
            .button(Button::new("add_row", "新增"))
            .button(Button::new("edit_row", "编辑"))
            .button(Button::new("delete_row", "删除"))
            .button(Button::new("clear_all", "清空"))
            .button(Button::new("from_config", "从配置导入"))
            .button(Button::new("preview", "预览变更"))
            .button(Button::new("refresh", "刷新分支"))
            .button(Button::new("sync", "同步全部"))
            .status(&status)
    }

    fn editor_view(&self, index: usize, root: &str) -> View {
        let m = &self.mappings[index];
        // The stored label stays an option even when the lookup failed, so an
        // unchanged row never silently falls back to the main branch/default module.
        let (branches, branch_selected) = branch_options(&self.branches, &self.e_branch);
        let (modules, module_selected) = module_options(&self.modules, &self.e_module);
        let mut code_labels: Vec<String> = self
            .module_labels()
            .into_iter()
            .map(|(label, _)| label)
            .collect();
        if code_labels.is_empty() {
            code_labels.push(self.code_label(&m.path));
        }
        let selected_code = if code_labels.contains(&self.e_code_module) {
            self.e_code_module.clone()
        } else {
            code_labels[0].clone()
        };
        let mut team_labels = vec!["（未选择）".to_string()];
        if !self.e_team.is_empty() && !self.teams.iter().any(|t| t.name == self.e_team) {
            team_labels.push(self.e_team.clone());
        }
        team_labels.extend(self.teams.iter().map(|t| t.name.clone()));
        let selected_team = if team_labels.contains(&self.e_team) {
            self.e_team.clone()
        } else {
            "（未选择）".to_string()
        };
        let mut project_labels = vec!["（未选择）".to_string()];
        if !self.e_project_pick.is_empty()
            && !self.projects.iter().any(|p| p.name == self.e_project_pick)
        {
            project_labels.push(self.e_project_pick.clone());
        }
        project_labels.extend(self.projects.iter().map(|p| p.name.clone()));
        let selected_project = if project_labels.contains(&self.e_project_pick) {
            self.e_project_pick.clone()
        } else {
            "（未选择）".to_string()
        };
        let ep_folders = folder_options(&self.endpoint_folders, m.endpoint_folder_id, &self.e_endpoint_folder);
        let sc_folders = folder_options(&self.schema_folders, m.schema_folder_id, &self.e_schema_folder);
        let fields = vec![
            Field::new(
                "e_code_module",
                "源代码模块",
                FieldValue::Choice(code_labels, selected_code),
            ),
            Field::new(
                "e_enabled",
                "启用",
                FieldValue::Checkbox(self.e_enabled),
            ),
            Field::new("e_team", "团队", FieldValue::Choice(team_labels, selected_team)),
            Field::new(
                "e_project_pick",
                "Apifox 项目",
                FieldValue::Choice(project_labels, selected_project),
            ),
            Field::new(
                "e_project",
                "Apifox 项目 ID（必填，数字；也可用上面的项目下拉回填）",
                FieldValue::Text(self.e_project.clone()),
            ),
            Field::new("e_branch", "分支 / 版本", FieldValue::Choice(branches, branch_selected)),
            Field::new("e_module", "模块", FieldValue::Choice(modules, module_selected)),
            Field::new(
                "e_endpoint_folder",
                "接口导入到",
                FieldValue::Choice(ep_folders.0, ep_folders.1),
            ),
            Field::new(
                "e_schema_folder",
                "数据模型导入到",
                FieldValue::Choice(sc_folders.0, sc_folders.1),
            ),
            Field::new(
                "e_endpoint_mode",
                "接口覆盖策略",
                FieldValue::Choice(
                    MODES.iter().map(|(l, _)| l.to_string()).collect(),
                    self.e_endpoint_mode.clone(),
                ),
            ),
            Field::new(
                "e_schema_mode",
                "数据模型覆盖策略",
                FieldValue::Choice(
                    MODES.iter().map(|(l, _)| l.to_string()).collect(),
                    self.e_schema_mode.clone(),
                ),
            ),
            Field::new(
                "e_naming",
                "参数命名转换",
                FieldValue::Choice(
                    NAMINGS.iter().map(|(l, _)| l.to_string()).collect(),
                    naming_label(&self.e_naming).to_string(),
                ),
            ),
            Field::new(
                "e_update_folder",
                "覆盖时同步更新接口目录",
                FieldValue::Checkbox(self.e_update_folder),
            ),
        ];
        let _ = root;
        View::new(Body::Form(fields))
            .button(Button::new("save", "保存"))
            .button(Button::new("reload_modules", "刷新模块"))
            .button(Button::new("reload_folders", "刷新目录"))
            .button(Button::new("reload_teams", "刷新团队"))
            .button(Button::new("reload", "刷新分支"))
            .button(Button::new("delete", "删除该行"))
            .button(Button::new("cancel", "返回列表"))
            .status(
                &self
                    .status
                    .clone()
                    .unwrap_or_else(|| "选分支会自动拉该分支的模块".into()),
            )
    }

    fn handle_table(
        &mut self,
        event: ViewEvent,
        root: &str,
        ctx: &Invocation,
    ) -> Result<(), Error> {
        if let Some(row) = event.table_select() {
            self.selected_row = Some(row as usize);
        }
        if let Some(row) = event.table_activate() {
            self.open_editor(row as usize, root, ctx)?;
            return Ok(());
        }
        match event {
            ViewEvent::Cell { row, column, value } => {
                if column == 0 {
                    if let Some(m) = self.mappings.get_mut(row as usize) {
                        m.enabled = matches!(value.trim(), "是" | "true" | "1" | "✓");
                        save_mappings(root, &self.mappings)?;
                    }
                } else if column == 1 {
                    self.status = Some("代码模块由「检测模块」生成".into());
                } else {
                    self.status = Some("双击行打开编辑（分支/模块是下拉框）".into());
                }
            }
            ViewEvent::InsertRow(row) => {
                let at = (row as usize).min(self.mappings.len());
                self.mappings.insert(at, Mapping { enabled: true, ..Default::default() });
                save_mappings(root, &self.mappings)?;
                self.open_editor(at, root, ctx)?;
            }
            ViewEvent::DeleteRows(rows) => {
                let mut idx: Vec<usize> = rows.into_iter().map(|r| r as usize).collect();
                idx.sort_unstable();
                idx.dedup();
                for i in idx.into_iter().rev() {
                    if i < self.mappings.len() {
                        self.mappings.remove(i);
                    }
                }
                save_mappings(root, &self.mappings)?;
                self.status = Some("已删除选中行".into());
            }
            ViewEvent::Button(id) => match id.as_str() {
                "detect" => self.detect(root, ctx)?,
                "add_row" => {
                    self.mappings.push(Mapping::default());
                    save_mappings(root, &self.mappings)?;
                    let row = self.mappings.len() - 1;
                    self.open_editor(row, root, ctx)?;
                }
                "edit_row" => {
                    match self.selected_row {
                        Some(row) if row < self.mappings.len() => self.open_editor(row, root, ctx)?,
                        _ => self.status = Some("先单击选中一行".into()),
                    }
                }
                "clear_all" => {
                    if host::confirm(
                        "清空映射表？",
                        &format!("将删除全部 {} 行，此操作不可撤销", self.mappings.len()),
                    )? {
                        self.mappings.clear();
                        save_mappings(root, &self.mappings)?;
                        self.selected_row = None;
                        self.status = Some("映射表已清空".into());
                    }
                }
                "delete_row" => match self.selected_row {
                    Some(row) if row < self.mappings.len() => {
                        self.mappings.remove(row);
                        save_mappings(root, &self.mappings)?;
                        self.selected_row = None;
                        self.status = Some("已删除选中行".into());
                    }
                    _ => self.status = Some("先单击选中一行".into()),
                },
                "preview" => self.preview(root, ctx)?,
                "from_config" => self.from_config(root, ctx)?,
                "refresh" => {
                    if let Some(pid) = self.mappings.iter().find_map(|m| {
                        (!m.project_id.is_empty()).then(|| m.project_id.clone())
                    }) {
                        self.refresh_lists(&pid);
                    } else {
                        self.status = Some("先在映射行里填 Apifox 项目 ID".into());
                    }
                }
                "sync" => {
                    if let Some(msg) = self.sync(ctx)? {
                        self.status = Some(msg);
                    }
                }
                _ => {}
            },
            _ => {}
        }
        Ok(())
    }

    fn handle_editor(
        &mut self,
        index: usize,
        event: ViewEvent,
        root: &str,
        ctx: &Invocation,
    ) -> Result<(), Error> {
        match event {
            ViewEvent::Field(key, value) => match key.as_str() {
                "e_code_module" => {
                    self.e_code_module = value;
                    return Ok(());
                }
                "e_project" => {
                    self.e_project = value.trim().to_string();
                    if self
                        .projects
                        .iter()
                        .find(|p| p.name == self.e_project_pick)
                        .is_none_or(|p| p.id.to_string() != self.e_project)
                    {
                        self.e_project_pick.clear();
                    }
                    // The keys of the project being typed are unknown: drop
                    // the previous project's choices so the pickers cannot
                    // offer (or silently keep) a destination from another one.
                    let project_changed = self
                        .editing
                        .and_then(|i| self.mappings.get(i))
                        .is_some_and(|m| m.project_id != self.e_project);
                    if project_changed {
                        self.branches.clear();
                        self.modules.clear();
                        self.endpoint_folders.clear();
                        self.schema_folders.clear();
                        self.e_endpoint_folder.clear();
                        self.e_schema_folder.clear();
                    }
                    return Ok(());
                }
                "e_team" => {
                    self.e_team = value.clone();
                    self.e_team_id = self.teams.iter().find(|t| t.name == value).map(|t| t.id);
                    self.e_project_pick.clear();
                    self.projects.clear();
                    if let Some(team) = self.teams.iter().find(|t| t.name == value) {
                        let id = team.id;
                        self.fetch_projects(id);
                    }
                }
                "e_project_pick" => {
                    self.e_project_pick = value.clone();
                    if let Some(project) = self.projects.iter().find(|p| p.name == value) {
                        self.e_project = project.id.to_string();
                        let id = project.id.to_string();
                        self.refresh_lists(&id);
                    } else {
                        self.e_project_pick.clear();
                    }
                }
                "e_enabled" => {
                    self.e_enabled = value == "true";
                }
                "e_branch" => {
                    self.e_branch = value.clone();
                    if let Some(b) = self.branches.iter().find(|b| b.label() == value) {
                        let pid = self.e_project.clone();
                        self.load_modules(&pid, Some(b.id));
                        self.fetch_folders();
                    }
                }
                "e_module" => {
                    self.e_module = value;
                    self.fetch_folders();
                }
                "e_endpoint_folder" => self.e_endpoint_folder = value,
                "e_schema_folder" => self.e_schema_folder = value,
                "e_endpoint_mode" => self.e_endpoint_mode = value,
                "e_schema_mode" => self.e_schema_mode = value,
                "e_naming" => self.e_naming = naming_value(&value).to_string(),
                "e_update_folder" => self.e_update_folder = value == "true",
                _ => return Ok(()),
            },
            ViewEvent::Button(id) => match id.as_str() {
                "save" => {
                    if self.e_project.is_empty() || !self.e_project.chars().all(|c| c.is_ascii_digit()) {
                        self.status = Some("Apifox 项目 ID 必填且必须是数字".into());
                        return Ok(());
                    }
                    let code_path = self.code_path(&self.e_code_module);
                    let code_name = self
                        .detected
                        .iter()
                        .find(|c| c.path == code_path)
                        .map(|c| c.name.clone())
                        .unwrap_or_default();
                    // Compare with the row as saved, before touching it: the
                    // project id may have been typed over, and the old
                    // project's branch/module/folder ids must not survive it.
                    let Some(saved) = self.mappings.get(index).cloned() else {
                        return Ok(());
                    };
                    let project_changed = saved.project_id != self.e_project;
                    if project_changed {
                        self.branches.clear();
                        self.modules.clear();
                        self.endpoint_folders.clear();
                        self.schema_folders.clear();
                        let pid = self.e_project.clone();
                        self.refresh_lists(&pid);
                        self.fetch_folders();
                    }
                    let (branch_id, branch_name) = match resolve_branch(
                        &self.branches,
                        &self.e_branch,
                        saved.branch_id,
                        &saved.branch_name,
                        !project_changed,
                    ) {
                        Ok(picked) => picked,
                        Err(e) => {
                            self.status = Some(e);
                            return Ok(());
                        }
                    };
                    let branch_unchanged = !project_changed && branch_id == saved.branch_id;
                    let (module_id, module_name) = match resolve_module(
                        &self.modules,
                        &self.e_module,
                        saved.module_id,
                        &saved.module_name,
                        branch_unchanged,
                    ) {
                        Ok(picked) => picked,
                        Err(e) => {
                            self.status = Some(e);
                            return Ok(());
                        }
                    };
                    // Folder ids belong to the project too; a typed-over
                    // project only accepts folders picked from its own list.
                    let (endpoint_folder_id, schema_folder_id) = if project_changed {
                        (
                            folder_id_by_path(&self.endpoint_folders, &self.e_endpoint_folder),
                            folder_id_by_path(&self.schema_folders, &self.e_schema_folder),
                        )
                    } else {
                        (
                            folder_id_by_label(&self.endpoint_folders, &self.e_endpoint_folder),
                            folder_id_by_label(&self.schema_folders, &self.e_schema_folder),
                        )
                    };
                    if let Some(m) = self.mappings.get_mut(index) {
                        m.enabled = self.e_enabled;
                        m.path = code_path;
                        m.name = code_name;
                        m.project_id = self.e_project.clone();
                        m.team_name = if self.e_team == "（未选择）" {
                            String::new()
                        } else {
                            self.e_team.clone()
                        };
                        m.team_id = self
                            .e_team_id
                            .or_else(|| self.teams.iter().find(|t| t.name == self.e_team).map(|t| t.id));
                        m.project_name = self.e_project_pick.clone();
                        if m.project_name.is_empty() {
                            if let Some(p) = self
                                .projects
                                .iter()
                                .find(|p| p.id.to_string() == self.e_project)
                            {
                                m.project_name = p.name.clone();
                            }
                        }
                        m.branch_id = branch_id;
                        m.branch_name = branch_name;
                        m.module_id = module_id;
                        m.module_name = module_name;
                        m.endpoint_folder_id = endpoint_folder_id;
                        m.schema_folder_id = schema_folder_id;
                        m.endpoint_mode = mode_value(&self.e_endpoint_mode).to_string();
                        m.schema_mode = mode_value(&self.e_schema_mode).to_string();
                        m.naming = self.e_naming.clone();
                        m.update_folder = self.e_update_folder;
                    }
                    save_mappings(root, &self.mappings)?;
                    self.editing = None;
                    self.status = Some("映射已保存".into());
                }
                "reload_folders" => {
                    self.fetch_folders();
                    self.status = Some(format!(
                        "目录：接口 {} 个，模型 {} 个",
                        self.endpoint_folders.len(),
                        self.schema_folders.len()
                    ));
                }
                "reload_modules" => {
                    self.detect(root, ctx)?;
                }
                "reload_teams" => {
                    self.fetch_teams();
                    self.status = Some(format!("已加载 {} 个团队", self.teams.len()));
                }
                "reload" => {
                    if self.e_project.is_empty() {
                        self.status = Some("先填 Apifox 项目 ID".into());
                    } else {
                        let pid = self.e_project.clone();
                        self.refresh_lists(&pid);
                        if self.e_team_id.is_none() {
                            self.resolve_team_for_project(&pid);
                        }
                    }
                }
                "delete" => {
                    if index < self.mappings.len() {
                        self.mappings.remove(index);
                    }
                    save_mappings(root, &self.mappings)?;
                    self.editing = None;
                    self.status = Some("已删除该映射".into());
                }
                "cancel" => {
                    self.editing = None;
                }
                _ => {}
            },
            _ => {}
        }
        Ok(())
    }

    fn open_editor(&mut self, row: usize, root: &str, ctx: &Invocation) -> Result<(), Error> {
        if row >= self.mappings.len() {
            return Ok(());
        }
        self.editing = Some(row);
        let m = self.mappings[row].clone();
        self.e_enabled = m.enabled;
        self.e_project = m.project_id.clone();
        self.e_branch = m.branch_label();
        self.e_module = m.module_label();
        self.e_endpoint_folder.clear();
        self.e_schema_folder.clear();
        self.e_endpoint_mode = mode_label(&m.endpoint_mode).to_string();
        self.e_schema_mode = mode_label(&m.schema_mode).to_string();
        self.e_naming = m.naming.clone();
        self.e_update_folder = m.update_folder;
        self.e_code_module = self.code_label(&m.path);
        if self.detected.is_empty() {
            self.detected = self.code_modules(root, ctx);
        }
        if self.teams.is_empty() {
            self.fetch_teams();
        }
        self.e_team_id = m
            .team_id
            .or_else(|| self.teams.iter().find(|t| t.name == m.team_name).map(|t| t.id));
        self.e_team = if !m.team_name.is_empty() {
            m.team_name.clone()
        } else {
            self.e_team_id
                .and_then(|id| self.teams.iter().find(|t| t.id == id))
                .map(|t| t.name.clone())
                .unwrap_or_default()
        };
        self.projects.clear();
        if let Some(team_id) = self.e_team_id {
            self.fetch_projects(team_id);
        }
        self.e_project_pick = if !m.project_name.is_empty() {
            m.project_name.clone()
        } else {
            self.projects
                .iter()
                .find(|p| p.id.to_string() == m.project_id)
                .map(|p| p.name.clone())
                .unwrap_or_default()
        };
        if self.e_team_id.is_none() && self.e_project_pick.is_empty() {
            self.resolve_team_for_project(&m.project_id);
        }
        self.branches.clear();
        self.modules.clear();
        if !self.e_project.is_empty() && host::secret_is_set(APIFOX_SECRET) {
            let pid = self.e_project.clone();
            self.refresh_lists(&pid);
        }
        self.fetch_folders();
        if let Some(id) = m.endpoint_folder_id {
            self.e_endpoint_folder = folder_label_by_id(&self.endpoint_folders, id);
        }
        if let Some(id) = m.schema_folder_id {
            self.e_schema_folder = folder_label_by_id(&self.schema_folders, id);
        }
        Ok(())
    }

    /// Compares the generated documents with the hashes stored by the last sync.
    fn preview(&mut self, root: &str, ctx: &Invocation) -> Result<(), Error> {
        let Some(project) = ctx.project() else {
            self.status = Some("先打开一个 Java 项目".into());
            return Ok(());
        };
        let rows: Vec<Mapping> = self
            .mappings
            .iter()
            .filter(|m| m.enabled && !m.project_id.is_empty())
            .cloned()
            .collect();
        if rows.is_empty() {
            self.status = Some("没有启用的映射行".into());
            return Ok(());
        }
        match docs_for(&project, root, &rows, &[]) {
            Ok(scan) => {
                let docs = scan.docs;
                let (mut changed, mut skipped) = (0, 0);
                for (row, doc) in rows.iter().zip(docs.iter()) {
                    if doc.endpoints.is_empty() {
                        skipped += 1;
                        continue;
                    }
                    let json = openapi::render(doc);
                    if stored_value(&hash_key(root, &row.path)) != fnv1a(&json) {
                        changed += 1;
                    }
                }
                let unchanged = rows.len() - changed - skipped;
                let mut status = format!(
                    "{changed} 个模块有变更，{unchanged} 个未变{}",
                    if skipped > 0 {
                        format!("，{skipped} 个无接口")
                    } else {
                        String::new()
                    }
                );
                if !scan.notes.is_empty() {
                    status.push_str("；");
                    status.push_str(&scan.notes.join("；"));
                }
                self.status = Some(status);
            }
            Err(e) => self.status = Some(format!("预览失败：{e}")),
        }
        Ok(())
    }

    /// `.apifox-sync.json` in the project root seeds the mapping table.
    fn from_config(&mut self, root: &str, ctx: &Invocation) -> Result<(), Error> {
        let Some(project) = ctx.project() else {
            self.status = Some("先打开一个 Java 项目".into());
            return Ok(());
        };
        let trimmed = root.trim_end_matches(['\\', '/']);
        let candidates = [
            format!("{trimmed}/.apifox-sync.json"),
            format!("{trimmed}\\.apifox-sync.json"),
        ];
        let mut text = None;
        for path in &candidates {
            if let Ok(content) = project.read_file(path) {
                text = Some(content);
                break;
            }
        }
        let Some(text) = text else {
            self.status = Some("项目根目录没有 .apifox-sync.json".into());
            return Ok(());
        };
        let value: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                self.status = Some(format!(".apifox-sync.json 解析失败：{e}"));
                return Ok(());
            }
        };
        let items = value
            .get("mappings")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if items.is_empty() {
            self.status = Some("配置里没有 mappings 数组".into());
            return Ok(());
        }
        self.mappings = items.iter().map(mapping_from).collect();
        save_mappings(root, &self.mappings)?;
        self.status = Some(format!("已从 .apifox-sync.json 导入 {} 行", self.mappings.len()));
        Ok(())
    }
    /// Refreshes the code-module list used by the editor dropdown. It never
    /// creates mapping rows: rows are added by the user with `+`.
    fn detect(&mut self, root: &str, ctx: &Invocation) -> Result<(), Error> {
        self.detected = self.code_modules(root, ctx);
        self.status = Some(if self.detected.is_empty() {
            "没检测到 pom.xml / settings.gradle".into()
        } else {
            format!(
                "检测到 {} 个源代码模块，可在「源代码模块」下拉里选择",
                self.detected.len()
            )
        });
        Ok(())
    }

    /// Build files -> code modules (also kept for the editor's module picker).
    /// Names only (no paths). Duplicated names get `(#n)` so a pick still maps
    /// to one module.
    fn module_labels(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for (i, module) in self.detected.iter().enumerate() {
            let duplicated = self.detected.iter().filter(|m| m.name == module.name).count() > 1;
            let label = if module.name.is_empty() {
                module.path.clone()
            } else if duplicated {
                format!("{} (#{})", module.name, i + 1)
            } else {
                module.name.clone()
            };
            out.push((label, module.path.clone()));
        }
        out
    }

    fn code_label(&self, path: &str) -> String {
        if path.is_empty() {
            return "（项目根）".to_string();
        }
        self.module_labels()
            .into_iter()
            .find(|(_, p)| p == path)
            .map(|(label, _)| label)
            .unwrap_or_else(|| path.to_string())
    }

    fn code_path(&self, label: &str) -> String {
        if label == "（项目根）" {
            return String::new();
        }
        self.module_labels()
            .into_iter()
            .find(|(l, _)| l == label)
            .map(|(_, path)| path)
            .unwrap_or_else(|| label.to_string())
    }

    fn code_modules(&self, root: &str, ctx: &Invocation) -> Vec<code_project::CodeModule> {
        let Some(project) = ctx.project() else {
            return Vec::new();
        };
        let mut build_files: Vec<(String, String)> = Vec::new();
        for pattern in ["**/pom.xml", "**/settings.gradle", "**/settings.gradle.kts"] {
            for path in project.find_files(pattern, 200).unwrap_or_default() {
                let rel = relative_to_root(root, &path);
                if let Ok(content) = project.read_file(&path) {
                    build_files.push((rel, content));
                }
            }
        }
        code_project::detect_modules(&build_files)
    }

    fn fetch_folders(&mut self) {
        let project_id = self.e_project.clone();
        if project_id.is_empty() {
            return;
        }
        let branch = self
            .branches
            .iter()
            .find(|b| b.label() == self.e_branch || b.name == self.e_branch)
            .map(|b| b.id);
        let module = self
            .modules
            .iter()
            .find(|m| m.name == self.e_module)
            .map(|m| m.id);
        self.endpoint_folders = load_folders(&self.server, &project_id, module, branch, false);
        self.schema_folders = load_folders(&self.server, &project_id, module, branch, true);
    }
    fn fetch_teams(&mut self) {
        if !host::secret_is_set(APIFOX_SECRET) {
            return;
        }
        if let Ok((status, text)) = call_apifox(&apifox::teams_request(), &self.server) {
            if let Ok(list) = apifox::parse_named(status, &text) {
                self.teams = list;
            }
        }
    }

    fn fetch_projects(&mut self, team_id: u64) {
        if let Ok((status, text)) =
            call_apifox(&apifox::projects_request(&team_id.to_string()), &self.server)
        {
            if let Ok(list) = apifox::parse_named(status, &text) {
                self.projects = list;
            }
        }
    }

    /// Older mappings only stored the numeric project id. Walk the teams once
    /// so the form can still show the team/project names.
    fn resolve_team_for_project(&mut self, project_id: &str) {
        if project_id.is_empty() || !host::secret_is_set(APIFOX_SECRET) {
            return;
        }
        if self.teams.is_empty() {
            self.fetch_teams();
        }
        for team in self.teams.clone() {
            self.fetch_projects(team.id);
            if let Some(project) = self.projects.iter().find(|p| p.id.to_string() == project_id) {
                self.e_team_id = Some(team.id);
                self.e_team = team.name;
                self.e_project_pick = project.name.clone();
                return;
            }
        }
    }
    fn refresh_lists(&mut self, project_id: &str) {
        if project_id.is_empty() {
            self.status = Some("先填 Apifox 项目 ID".into());
            return;
        }
        if !host::secret_is_set(APIFOX_SECRET) {
            self.status = Some("先在设置页保存访问令牌".into());
            return;
        }
        let branches = match call_apifox(&apifox::branches_request(project_id), &self.server) {
            Ok((status, text)) => match apifox::parse_branches(status, &text) {
                Ok(v) => v,
                Err(e) => {
                    self.status = Some(format!("分支列表读取失败：{e}"));
                    return;
                }
            },
            Err(e) => {
                self.status = Some(format!("分支列表请求失败：{e}"));
                return;
            }
        };
        self.branches = branches;
        let wanted = self.e_branch.clone();
        let found = self
            .branches
            .iter()
            .find(|b| b.label() == wanted || b.name == wanted)
            .map(|b| (b.id, b.label()));
        let branch_id = found.as_ref().map(|(id, _)| *id);
        if let Some((_, label)) = found {
            self.e_branch = label;
        }
        self.modules.clear();
        if !self.branches.is_empty() {
            self.load_modules(project_id, branch_id);
        }
        self.status = Some(if self.branches.is_empty() {
            "未读到分支（检查项目 ID/令牌权限）".to_string()
        } else {
            format!("已加载 {} 个分支、{} 个模块", self.branches.len(), self.modules.len())
        });
    }

    fn load_modules(&mut self, project_id: &str, branch_id: Option<u64>) {
        if project_id.is_empty() {
            return;
        }
        if let Ok((status, text)) = call_apifox(&apifox::modules_request(project_id, branch_id), &self.server) {
            if let Ok(list) = apifox::parse_modules(status, &text) {
                self.modules = list;
            }
        }
    }
}

impl Main {
    /// Scans the enabled mapping rows, imports each module and records the
    /// result in `self.report`, which the `report` view shows (and refreshes
    /// after every module, so the upload can be watched as it runs).
    fn sync(&mut self, ctx: &Invocation) -> Result<Option<String>, Error> {
    let project = ctx.project().ok_or("请先打开一个 Java 项目再运行该命令")?;
    let root = project.root().unwrap_or_default();
    let mappings = load_mappings(&root);
    let mut rows: Vec<Mapping> = mappings
        .into_iter()
        .filter(|m| m.enabled && !m.project_id.is_empty())
        .collect();
    // Right-clicking a controller/folder limits the sync to its module and,
    // below, to the selected files.
    let selected: Vec<String> = ctx
        .selected_paths()
        .iter()
        .map(|p| relative_to_root(&root, p))
        .filter(|p| !p.is_empty())
        .collect();
    if !selected.is_empty() {
        let matched: Vec<Mapping> = rows
            .iter()
            .filter(|m| !m.path.is_empty() && row_matches(&m.path, &selected))
            .cloned()
            .collect();
        if !matched.is_empty() {
            rows = matched;
        }
    }
    if rows.is_empty() {
        return Err("映射表没有可同步的行：填 Apifox 项目 ID 并勾选启用".into());
    }
    if !ensure_token()? {
        return Ok(None);
    }

    let ScanOutcome {
        docs,
        notes: scan_notes,
        files,
        module_files,
    } = docs_for(&project, &root, &rows, &selected)?;
    // `server.servlet.context-path` from the nearest application.yml/properties.
    let base = server_base(&root);

    let task = host::begin_task("扫描模块并同步到 Apifox", false);
    let mut created = 0u64;
    let mut updated = 0u64;
    let mut failed = 0u64;
    let mut hints: Vec<String> = scan_notes;
    let mut notes: Vec<String> = Vec::new();
    let mut done = 0usize;
    let mut report = ImportReport {
        scanned: format!("扫描 {files} 个 Java 文件（映射模块内 {module_files} 个）"),
        ..Default::default()
    };
    // Open the result panel first: each module appears there as it finishes.
    self.report = Some(report.clone());
    let _ = host::reveal_view("report");
    host::refresh_view("report");

    for (row, doc) in rows.iter().zip(docs.into_iter()) {
        if task.is_cancelled() {
            return Err(Error::Cancelled);
        }
        done += 1;
        task.report(done as u32, Some(rows.len() as u32), Some(row.name.as_str()));

        let mut entry = ImportEntry {
            module: if row.name.is_empty() {
                row.path.clone()
            } else {
                row.name.clone()
            },
            path: row.path.clone(),
            project_id: row.project_id.clone(),
            project_name: if row.project_name.is_empty() {
                "—".to_string()
            } else {
                row.project_name.clone()
            },
            branch: row.branch_label(),
            apifox_module: row.module_label(),
            endpoint_folder: folder_report_label(row.endpoint_folder_id),
            schema_folder: folder_report_label(row.schema_folder_id),
            endpoints: doc.endpoints.len(),
            endpoint_list: doc
                .endpoints
                .iter()
                .take(20)
                .map(|e| {
                    if e.summary.is_empty() {
                        format!("{} {}", e.method, e.path)
                    } else {
                        format!("{} {} — {}", e.method, e.path, e.summary)
                    }
                })
                .collect(),
            outcome: None,
            note: None,
        };

        if doc.endpoints.is_empty() {
            let note = format!(
                "没有接口，跳过（项目 {} 个 Java 文件，映射模块内 {} 个）",
                files, module_files
            );
            hints.push(format!("{}：{note}", row.name));
            entry.note = Some(note);
            report.entries.push(entry);
            self.report = Some(report.clone());
            host::refresh_view("report");
            continue;
        }
        let json = openapi::render(&doc);
        let opts = apifox::ImportOptions {
            branch_id: row.branch_id,
            module_id: row.module_id,
            endpoint_folder_id: row.endpoint_folder_id,
            schema_folder_id: row.schema_folder_id,
            endpoint_mode: row.endpoint_mode.clone(),
            schema_mode: row.schema_mode.clone(),
            update_folder: row.update_folder,
        };
        let request = apifox::import_request(&row.project_id, &json, &opts);
        let url = rewrite_server(&request.url, &base);
        let mut req = HttpRequest::post(&url, request.body.clone());
        for (name, value) in &request.headers {
            req = req.header(name.as_str(), value.as_str());
        }
        req.timeout_ms = Some(60_000);
        match fetch_with_retry(&req) {
            Ok(response) => match apifox::parse_response(response.status, &response.text()) {
                Ok(out) => {
                    let _ = host::storage::set(&hash_key(&root, &row.path), &fnv1a(&json));
                    created += out.created;
                    updated += out.updated;
                    failed += out.failed;
                    report.created += out.created;
                    report.updated += out.updated;
                    report.ignored += out.ignored;
                    report.failed += out.failed;
                    report.schemas_created += out.schemas_created;
                    report.schemas_updated += out.schemas_updated;
                    if !out.errors.is_empty() {
                        notes.push(format!("{}：{}", row.name, out.errors.join("；")));
                    }
                    entry.outcome = Some(out);
                }
                Err(e) => {
                    failed += 1;
                    report.failed += 1;
                    notes.push(format!("{}：{e}", row.name));
                    entry.note = Some(e);
                }
            },
            Err(e) => {
                failed += 1;
                report.failed += 1;
                notes.push(format!("{}：{e}", row.name));
                entry.note = Some(e.to_string());
            }
        }
        report.entries.push(entry);
        self.report = Some(report.clone());
        host::refresh_view("report");
    }

    // The same lines the report view shows, in the IDE log (Help → Show Log).
    for entry in &report.entries {
        let line = match (&entry.outcome, &entry.note) {
            (Some(out), _) => format!(
                "{} → 项目 {}/{}, {}, {}: {}",
                entry.module,
                entry.project_id,
                entry.project_name,
                entry.branch,
                entry.apifox_module,
                out.summary()
            ),
            (None, Some(note)) => format!("{} → 未导入：{note}", entry.module),
            (None, None) => format!("{} → 未导入", entry.module),
        };
        host::log(
            if entry.outcome.is_some() {
                LogLevel::Info
            } else {
                LogLevel::Warn
            },
            &format!("{TITLE} {line}"),
        );
    }

    let mut message = format!(
        "同步 {} 个模块：新建 {}，更新 {}，失败 {}",
        rows.len(),
        created,
        updated,
        failed
    );
    if !hints.is_empty() {
        message.push_str("；");
        message.push_str(&hints.join("；"));
    }
    if !notes.is_empty() {
        message.push_str("；");
        message.push_str(&notes.join("；"));
    }
    let level = if failed > 0 || !notes.is_empty() {
        NotificationLevel::Warning
    } else {
        NotificationLevel::Info
    };
    host::notify(level, TITLE, &message);
    self.report = Some(report);
    host::refresh_view("report");
    Ok(Some(message))
}
}

fn project_document(ctx: &Invocation) -> Result<(Project, apifox_core::model::ApiDoc), Error> {
    let project = ctx.project().ok_or("请先打开一个 Java 项目再运行该命令")?;
    let root = project.root().unwrap_or_default();
    let files = project.find_files("**/*.java", MAX_FILES)?;
    let mut rels: Vec<String> = Vec::new();
    let mut locations: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for path in &files {
        let rel = relative_to_root(&root, path);
        if !is_doc_source(&rel) {
            continue;
        }
        locations.insert(rel.clone(), path.clone());
        rels.push(rel);
    }
    let doc = scan_modules_scoped(
        &rels,
        |rel| locations.get(rel).and_then(|path| project.read_file(path).ok()),
        &[String::new()],
        &[],
    )
    .into_iter()
    .next()
    .expect("one document per module");
    if doc.endpoints.is_empty() {
        return Err("未找到 Spring Controller".into());
    }
    Ok((project, doc))
}

fn export(ctx: &Invocation) -> Result<Option<String>, Error> {
    let (project, doc) = project_document(ctx)?;
    let json = openapi::render(&doc);
    project.write_file(OPENAPI_PATH, &json)?;
    let message = format!("已导出 {} 个接口到 {OPENAPI_PATH}", doc.endpoints.len());
    host::notify(NotificationLevel::Info, TITLE, &message);
    Ok(Some(message))
}

fn export_yaml(ctx: &Invocation) -> Result<Option<String>, Error> {
    let (project, doc) = project_document(ctx)?;
    let json: serde_json::Value = serde_json::from_str(&openapi::render(&doc))
        .map_err(|e| format!("生成 OpenAPI 失败：{e}"))?;
    let text = yaml::to_yaml(&json);
    project.write_file("docs/openapi.yaml", &text)?;
    let message = format!("已导出 {} 个接口到 docs/openapi.yaml", doc.endpoints.len());
    host::notify(NotificationLevel::Info, TITLE, &message);
    Ok(Some(message))
}

fn export_postman(ctx: &Invocation) -> Result<Option<String>, Error> {
    let (project, doc) = project_document(ctx)?;
    let text = postman::collection(&doc, "API 文档");
    project.write_file("docs/postman_collection.json", &text)?;
    let message = format!(
        "已导出 {} 个接口到 docs/postman_collection.json",
        doc.endpoints.len()
    );
    host::notify(NotificationLevel::Info, TITLE, &message);
    Ok(Some(message))
}

fn configure(ctx: &Invocation) -> Result<Option<String>, Error> {
    let project = ctx.project().ok_or("请先打开一个 Java 项目再运行该命令")?;
    let _ = project;
    if host::secret_is_set(APIFOX_SECRET) {
        if host::confirm("重新设置 Apifox 访问令牌？", "选「是」可替换或删除已保存的令牌")? {
            if let Some(raw) = host::input("输入新的访问令牌（留空 = 删除）", None)? {
                let raw = raw.trim();
                if raw.is_empty() {
                    host::secret_delete(APIFOX_SECRET)?;
                } else {
                    let token = raw.strip_prefix("Bearer ").unwrap_or(raw).trim();
                    host::secret_set(APIFOX_SECRET, token)?;
                }
            }
        }
    } else if let Some(raw) = host::input("输入 Apifox 访问令牌（留空跳过）", None)? {
        let raw = raw.trim();
        if !raw.is_empty() {
            let token = raw.strip_prefix("Bearer ").unwrap_or(raw).trim();
            host::secret_set(APIFOX_SECRET, token)?;
        }
    }
    let message = "令牌设置已更新；项目/分支/模块请在「映射」标签按代码模块配置";
    host::notify(NotificationLevel::Info, TITLE, message);
    Ok(Some(message.to_string()))
}

fn ensure_token() -> Result<bool, Error> {
    if host::secret_is_set(APIFOX_SECRET) {
        return Ok(true);
    }
    let Some(raw) = host::input(
        "首次同步需要 Apifox 访问令牌（Apifox → 账号设置 → API 访问令牌）",
        None,
    )?
    else {
        return Ok(false);
    };
    let raw = raw.trim();
    let token = raw.strip_prefix("Bearer ").unwrap_or(raw).trim();
    if token.is_empty() {
        return Ok(false);
    }
    host::secret_set(APIFOX_SECRET, token)?;
    Ok(true)
}



/// A transport-level failure worth retrying: the host's HTTP client
/// (reqwest/rustls) can surface a stale keep-alive connection as e.g.
/// `peer closed connection without sending TLS close_notify`, `connection
/// reset`, or a timeout. Retrying on a fresh connection normally succeeds.
fn is_transient_fetch_error(e: &Error) -> bool {
    let msg = e.to_string().to_ascii_lowercase();
    [
        "close_notify",
        "connection error",
        "connection reset",
        "connection closed",
        "sendrequest",
        "error sending request",
        "timed out",
        "timeout",
        "unexpected eof",
        "peer closed",
    ]
    .iter()
    .any(|k| msg.contains(k))
}

/// `host::fetch` with a small retry for transient connection failures.
/// No sleep is available to a wasm plugin; discarding a poisoned pooled
/// connection is enough for the next attempt to open a fresh one.
fn fetch_with_retry(req: &HttpRequest) -> Result<openidea_plugin::HttpResponse, Error> {
    let mut last: Option<Error> = None;
    for _ in 0..3 {
        match host::fetch(req) {
            Ok(response) => return Ok(response),
            Err(e) => {
                if !is_transient_fetch_error(&e) {
                    return Err(e);
                }
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| Error::Failed("fetch failed".into())))
}

fn call_apifox(call: &apifox::ApiCall, base: &str) -> Result<(u16, String), Error> {
    let url = rewrite_server(&call.url, base);
    let mut req = if call.method == "GET" {
        HttpRequest::get(&url)
    } else {
        HttpRequest::post(&url, call.body.clone().unwrap_or_default())
    };
    for (name, value) in &call.headers {
        req = req.header(name.as_str(), value.as_str());
    }
    req.timeout_ms = Some(30_000);
    let response = fetch_with_retry(&req)?;
    Ok((response.status, response.text()))
}

/// Build output and tests are never API sources: skipping them keeps a big
/// monorepo (thousands of generated `.java`) inside the host CPU budget.
fn is_doc_source(path: &str) -> bool {
    let p = path.replace('\\', "/").to_ascii_lowercase();
    const SKIP: &[&str] = &["/target/", "/build/", "/out/", "/.git/", "/node_modules/", "/.idea/"];
    !SKIP.iter().any(|s| p.contains(s)) && !p.contains("/src/test/")
}
fn relative_to_root(root: &str, path: &str) -> String {
    let root = root.replace('\\', "/");
    let path = path.replace('\\', "/");
    let trimmed = root.trim_end_matches('/');
    let rel = if !trimmed.is_empty() {
        path.strip_prefix(trimmed).unwrap_or(&path)
    } else {
        &path
    };
    rel.trim_start_matches('/').to_string()
}

/* ---------- storage ---------- */

fn server_key(root: &str) -> String {
    format!("apifox.server:{root}")
}

fn server_base(root: &str) -> String {
    let saved = stored_value(&server_key(root));
    if saved.is_empty() {
        DEFAULT_SERVER.to_string()
    } else {
        saved
    }
}

fn rewrite_server(url: &str, base: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.is_empty() || base == DEFAULT_SERVER {
        url.to_string()
    } else {
        url.replacen(DEFAULT_SERVER, base, 1)
    }
}
fn mappings_key(root: &str) -> String {
    format!("apifox.mappings:{root}")
}

fn load_mappings(root: &str) -> Vec<Mapping> {
    if let Some(raw) = host::storage::get(&mappings_key(root)).ok().flatten() {
        if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&raw) {
            return items.iter().map(mapping_from).collect();
        }
    }
    migrate_legacy(root)
}

fn mapping_from(v: &Value) -> Mapping {
    Mapping {
        enabled: v.get("enabled").and_then(Value::as_bool).unwrap_or(true),
        name: text(v, "name"),
        path: text(v, "path"),
        project_id: text(v, "projectId"),
        team_id: v.get("teamId").and_then(Value::as_u64),
        team_name: text(v, "teamName"),
        project_name: text(v, "projectName"),
        branch_id: v.get("branchId").and_then(Value::as_u64),
        branch_name: text(v, "branchName"),
        module_id: v.get("moduleId").and_then(Value::as_u64),
        module_name: text(v, "moduleName"),
        endpoint_folder_id: v.get("endpointFolderId").and_then(Value::as_u64),
        schema_folder_id: v.get("schemaFolderId").and_then(Value::as_u64),
        endpoint_mode: mode_or_default(text(v, "endpointMode")),
        schema_mode: mode_or_default(text(v, "schemaMode")),
        naming: text(v, "naming"),
        update_folder: v.get("updateFolder").and_then(Value::as_bool).unwrap_or(false),
    }
}

const NAMINGS: [(&str, &str); 7] = [
    ("保持原样", "original"),
    ("小驼峰", "camel"),
    ("大驼峰", "pascal"),
    ("下划线", "snake"),
    ("短横线", "kebab"),
    ("全小写", "flat"),
    ("全大写", "upper"),
];

fn naming_label(value: &str) -> &'static str {
    NAMINGS
        .iter()
        .find(|(_, v)| *v == value)
        .map(|(l, _)| *l)
        .unwrap_or("保持原样")
}

fn naming_value(label: &str) -> &'static str {
    NAMINGS
        .iter()
        .find(|(l, _)| *l == label)
        .map(|(_, v)| *v)
        .unwrap_or("original")
}

const DEFAULT_SERVER: &str = "https://api.apifox.com";
const MODES: [(&str, &str); 4] = [
    ("智能合并", "AUTO_MERGE"),
    ("覆盖所有字段", "OVERWRITE_EXISTING"),
    ("不导入", "KEEP_EXISTING"),
    ("保留两者", "CREATE_NEW"),
];

fn load_folders(
    server: &str,
    project_id: &str,
    module: Option<u64>,
    branch: Option<u64>,
    schema: bool,
) -> Vec<apifox::Folder> {
    match call_apifox(&apifox::folders_request(project_id, module, branch, schema), server) {
        Ok((status, text)) => apifox::parse_folders(status, &text).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Branch choices for the editor. A stored label that is not in the fetched
/// list stays selectable: the lists are cleared when the editor opens and may
/// fail to load, and dropping the label would silently save the main branch.
fn branch_options(list: &[apifox::Branch], current: &str) -> (Vec<String>, String) {
    let mut options = vec![NO_BRANCH.to_string()];
    options.extend(list.iter().map(|b| b.label()));
    // A row stores the branch name; the picker shows `name（迭代分支）`.
    let selected = if let Some(b) = list.iter().find(|b| b.name == current || b.label() == current) {
        b.label()
    } else if !current.is_empty() && current != NO_BRANCH {
        if !options.iter().any(|o| o == current) {
            options.push(current.to_string());
        }
        current.to_string()
    } else {
        NO_BRANCH.to_string()
    };
    (options, selected)
}

/// Module choices; same rule as `branch_options`.
fn module_options(list: &[apifox::Module], current: &str) -> (Vec<String>, String) {
    let mut options = vec![DEFAULT_MODULE.to_string()];
    options.extend(list.iter().map(|m| m.name.clone()));
    if !current.is_empty() && !options.iter().any(|o| o == current) {
        options.push(current.to_string());
    }
    let selected = if options.iter().any(|o| o == current) {
        current.to_string()
    } else {
        DEFAULT_MODULE.to_string()
    };
    (options, selected)
}

/// What the saved row should keep for its branch. Unchanged text keeps the
/// stored id even when the list is missing; anything else must be validated
/// against the list, so a failed lookup can never turn "feature/41" into
/// "main" (`branchId: null`) behind the user's back.
fn resolve_branch(
    list: &[apifox::Branch],
    chosen: &str,
    saved_id: Option<u64>,
    saved_name: &str,
    project_unchanged: bool,
) -> Result<(Option<u64>, String), String> {
    if let Some(b) = list.iter().find(|b| b.name == chosen || b.label() == chosen) {
        return Ok((Some(b.id), b.name.clone()));
    }
    let saved_label = if saved_name.is_empty() { NO_BRANCH } else { saved_name };
    if chosen == saved_label && project_unchanged {
        return Ok((saved_id, saved_name.to_string()));
    }
    if chosen == NO_BRANCH {
        return Ok((None, String::new()));
    }
    Err(if list.is_empty() {
        format!("分支列表未加载，无法校验「{chosen}」；点「刷新分支」后重试")
    } else {
        format!("分支「{chosen}」不在当前项目里；请重新选择，或显式选择「{NO_BRANCH}」")
    })
}

/// Module counterpart of `resolve_branch`. A branch change invalidates the
/// stored module id (module ids are branch-scoped), so an unchanged label may
/// only be kept while the branch itself did not change.
fn resolve_module(
    list: &[apifox::Module],
    chosen: &str,
    saved_id: Option<u64>,
    saved_name: &str,
    branch_unchanged: bool,
) -> Result<(Option<u64>, String), String> {
    if let Some(m) = list.iter().find(|m| m.name == chosen) {
        return Ok((Some(m.id), m.name.clone()));
    }
    let saved_label = if saved_name.is_empty() { DEFAULT_MODULE } else { saved_name };
    if chosen == saved_label && branch_unchanged {
        return Ok((saved_id, saved_name.to_string()));
    }
    if chosen == DEFAULT_MODULE {
        return Ok((None, String::new()));
    }
    Err(if list.is_empty() {
        format!("模块列表未加载，无法校验「{chosen}」；点「刷新模块」后重试")
    } else {
        format!("模块「{chosen}」不在当前分支里；请重新选择，或显式选择「{DEFAULT_MODULE}」")
    })
}

fn folder_label_by_id(list: &[apifox::Folder], id: u64) -> String {
    list.iter()
        .find(|f| f.id == id)
        .map(|f| f.path.clone())
        .unwrap_or_else(|| format!("ID {id}"))
}

fn folder_id_by_label(list: &[apifox::Folder], label: &str) -> Option<u64> {
    let label = label.trim();
    if label.is_empty() || label == "根目录" {
        return None;
    }
    if let Some(rest) = label.strip_prefix("ID ") {
        return rest.trim().parse().ok();
    }
    list.iter().find(|f| f.path == label).map(|f| f.id)
}

/// Like `folder_id_by_label`, but only accepts a path that exists in `list`:
/// after a project change an `ID n` label points at the other project.
fn folder_id_by_path(list: &[apifox::Folder], label: &str) -> Option<u64> {
    let label = label.trim();
    if label.is_empty() || label == "根目录" || label.starts_with("ID ") {
        return None;
    }
    list.iter().find(|f| f.path == label).map(|f| f.id)
}

fn folder_options(
    list: &[apifox::Folder],
    current: Option<u64>,
    preferred: &str,
) -> (Vec<String>, String) {
    let mut options = vec!["根目录".to_string()];
    options.extend(list.iter().map(|f| f.path.clone()));
    if !preferred.is_empty() && options.iter().any(|o| o == preferred) {
        return (options, preferred.to_string());
    }
    match current {
        None => (options, "根目录".to_string()),
        Some(id) => {
            let label = folder_label_by_id(list, id);
            if !options.iter().any(|o| o == &label) {
                options.push(label.clone());
            }
            (options, label)
        }
    }
}
/// Minimal HTML escape for the report view (sandboxed frame, no template engine).
fn esc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Folder for the report: names are only known while a project is loaded, so
/// the report shows the root or the id it imported into.
fn folder_report_label(id: Option<u64>) -> String {
    match id {
        None => "根目录".to_string(),
        Some(id) => format!("目录 #{id}"),
    }
}

fn folder_label(id: Option<u64>) -> String {
    id.map(|v| v.to_string()).unwrap_or_else(|| "根目录".into())
}


fn mode_or_default(value: String) -> String {
    if MODES.iter().any(|(_, m)| *m == value) {
        value
    } else {
        "AUTO_MERGE".into()
    }
}

fn mode_label(value: &str) -> &'static str {
    MODES
        .iter()
        .find(|(_, m)| *m == value)
        .map(|(l, _)| *l)
        .unwrap_or("智能合并")
}

fn mode_value(label: &str) -> &'static str {
    MODES
        .iter()
        .find(|(l, _)| *l == label)
        .map(|(_, m)| *m)
        .unwrap_or("AUTO_MERGE")
}
fn stored_value(key: &str) -> String {
    host::storage::get(key)
        .ok()
        .flatten()
        .unwrap_or_default()
        .trim()
        .to_string()
}
fn text(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn save_mappings(root: &str, list: &[Mapping]) -> Result<(), Error> {
    let items: Vec<Value> = list
        .iter()
        .map(|m| {
            json!({
                "enabled": m.enabled,
                "name": m.name,
                "path": m.path,
                "projectId": m.project_id,
                "teamId": m.team_id,
                "teamName": m.team_name,
                "projectName": m.project_name,
                "branchId": m.branch_id,
                "branchName": m.branch_name,
                "moduleId": m.module_id,
                "moduleName": m.module_name,
                "endpointFolderId": m.endpoint_folder_id,
                "schemaFolderId": m.schema_folder_id,
                "endpointMode": m.endpoint_mode,
                "schemaMode": m.schema_mode,
                "naming": m.naming,
                "updateFolder": m.update_folder,
            })
        })
        .collect();
    host::storage::set(&mappings_key(root), &Value::Array(items).to_string())
}

/// One row from the 0.3.x single-project settings, so upgrades do not lose it.
fn migrate_legacy(root: &str) -> Vec<Mapping> {
    let project = host::storage::get(&format!("apifox.project_id:{root}"))
        .ok()
        .flatten()
        .unwrap_or_default();
    let branch = host::storage::get(&format!("apifox.branch:{root}"))
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());
    let module = host::storage::get(&format!("apifox.module:{root}:0"))
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());
    if project.trim().is_empty() {
        return Vec::new();
    }
    vec![Mapping {
        enabled: true,
        name: root.rsplit(['\\', '/']).find(|s| !s.is_empty()).unwrap_or("项目").to_string(),
        path: String::new(),
        project_id: project.trim().to_string(),
        branch_id: branch.as_ref().and_then(|v| v.get("id")).and_then(Value::as_u64),
        branch_name: branch
            .as_ref()
            .and_then(|v| v.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        module_id: module.as_ref().and_then(|v| v.get("id")).and_then(Value::as_u64),
        module_name: module
            .as_ref()
            .and_then(|v| v.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        ..Mapping::default()
    }]
}

/// Documents for `rows`: endpoints per module, types from the whole project,
/// with context-path and naming applied. Shared by sync and preview.
struct ScanOutcome {
    docs: Vec<apifox_core::model::ApiDoc>,
    /// Reasons the result differs from what the mapping table literally says
    /// (selection ignored, module path no longer on disk). Shown to the user.
    notes: Vec<String>,
    /// Java files read from the project, and how many live in the modules.
    files: usize,
    module_files: usize,
}

fn docs_for(
    project: &Project,
    root: &str,
    rows: &[Mapping],
    selected: &[String],
) -> Result<ScanOutcome, Error> {
    let files = project.find_files("**/*.java", MAX_FILES)?;
    let mut rels: Vec<String> = Vec::new();
    let mut locations: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for path in &files {
        let rel = relative_to_root(root, path);
        if !is_doc_source(&rel) {
            continue;
        }
        locations.insert(rel.clone(), path.clone());
        rels.push(rel);
    }
    let root_name = root.rsplit(['\\', '/']).find(|s| !s.is_empty()).unwrap_or("");
    let mut notes: Vec<String> = Vec::new();
    let paths: Vec<String> = rows
        .iter()
        .map(|row| {
            let scope = apifox_core::module_scope(&rels, &row.path, root_name);
            if scope.is_empty() && !row.path.is_empty() {
                notes.push(format!(
                    "{}：映射路径 {} 下没有文件，已按项目根目录扫描",
                    row.name, row.path
                ));
            }
            scope
        })
        .collect();
    let scan = apifox_core::scan_modules_scoped_report(
        &rels,
        |rel| locations.get(rel).and_then(|path| project.read_file(path).ok()),
        &paths,
        selected,
    );
    if scan.selection_ignored {
        notes.push(format!(
            "选中项里没有接口，已按整个模块同步（模块内 {} 个 Java 文件）",
            scan.module_files
        ));
    }
    let mut docs = scan.docs;
    let contexts = context_paths(project, root);
    for (row, doc) in rows.iter().zip(docs.iter_mut()) {
        if let Some(prefix) = context_for(&row.path, &contexts) {
            prefix_paths(doc, &prefix);
        }
        apifox_core::naming::apply(doc, apifox_core::naming::Naming::parse(&row.naming));
    }
    Ok(ScanOutcome {
        docs,
        notes,
        files: scan.files,
        module_files: scan.module_files,
    })
}

fn row_matches(module: &str, selected: &[String]) -> bool {
    let module = module.trim_end_matches('/');
    selected.iter().any(|path| {
        let path = path.trim_end_matches('/');
        path == module || path.starts_with(&format!("{module}/"))
    })
}
fn hash_key(root: &str, module: &str) -> String {
    format!("apifox.hash:{root}:{module}")
}

fn fnv1a(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}
/// `(directory, context-path)` pairs from Spring Boot config files.
fn context_paths(project: &Project, root: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for pattern in [
        "**/application.properties",
        "**/application.yml",
        "**/application.yaml",
        "**/bootstrap.properties",
        "**/bootstrap.yml",
        "**/bootstrap.yaml",
    ] {
        for path in project.find_files(pattern, 200).unwrap_or_default() {
            let Ok(text) = project.read_file(&path) else { continue };
            let Some(prefix) = read_context(&text) else { continue };
            let rel = relative_to_root(root, &path);
            let dir = rel.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
            if !out.iter().any(|(d, _)| d == &dir) {
                out.push((dir, prefix));
            }
        }
    }
    out
}

fn read_context(text: &str) -> Option<String> {
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') || t.starts_with("//") {
            continue;
        }
        for key in ["server.servlet.context-path", "server.context-path"] {
            if let Some(rest) = t.strip_prefix(key) {
                if let Some(value) = rest.trim_start().strip_prefix('=') {
                    let value = clean_value(value);
                    if !value.is_empty() {
                        return Some(value);
                    }
                }
            }
        }
        if let Some(rest) = t.strip_prefix("context-path:") {
            let value = clean_value(rest);
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

fn clean_value(value: &str) -> String {
    value.trim().trim_matches('"').trim_matches('\'').to_string()
}

/// The config whose directory shares the most path segments with the module.
fn context_for(module: &str, contexts: &[(String, String)]) -> Option<String> {
    let mut best: Option<(usize, &str)> = None;
    for (dir, prefix) in contexts {
        let score = dir
            .split('/')
            .filter(|s| !s.is_empty())
            .zip(module.split('/').filter(|s| !s.is_empty()))
            .take_while(|(a, b)| a == b)
            .count();
        if best.map(|(s, _)| score > s).unwrap_or(true) {
            best = Some((score, prefix));
        }
    }
    best.map(|(_, prefix)| prefix.to_string())
}

register_plugin!(Main, views);

#[cfg(test)]
mod tests {
    use super::*;
    use openidea_plugin::mock::MockHost;
    use openidea_plugin::HttpResponse;

    #[test]
    fn transient_fetch_errors_are_detected() {
        let rustls = Error::Failed("error sending request for url (https://api.apifox.com/v1/projects/2418666/import-openapi): client error (SendRequest): connection error: peer closed connection without sending TLS close_notify".into());
        assert!(is_transient_fetch_error(&rustls));
        assert!(is_transient_fetch_error(&Error::Failed("Operation timed out".into())));
        assert!(is_transient_fetch_error(&Error::Failed("connection reset by peer".into())));
        // A real API error (HTTP-level) must not be retried.
        assert!(!is_transient_fetch_error(&Error::Failed("Apifox：invalid token".into())));
        assert!(!is_transient_fetch_error(&Error::Invalid("bad url".into())));
    }

    fn html_of(report: ImportReport) -> String {
        let main = Main { report: Some(report), ..Default::default() };
        match main.report_view().body {
            Body::Html(html) => html,
            other => panic!("report view must be HTML, got {other:?}"),
        }
    }

    fn entry(name: &str, outcome: Option<apifox::ImportOutcome>, note: Option<&str>) -> ImportEntry {
        ImportEntry {
            module: name.into(),
            path: format!("erp/{name}"),
            project_id: "8957701".into(),
            project_name: "ERP".into(),
            branch: "feature".into(),
            apifox_module: "(默认模块)".into(),
            endpoint_folder: "根目录".into(),
            schema_folder: "目录 #12".into(),
            endpoints: 2,
            endpoint_list: vec!["GET /report/profit — 利润报表".into()],
            outcome,
            note: note.map(str::to_string),
        }
    }

    #[test]
    fn report_lists_each_module_and_totals_like_the_reference() {
        let html = html_of(ImportReport {
            entries: vec![
                entry(
                    "saas-erp",
                    Some(apifox::ImportOutcome {
                        created: 0,
                        updated: 2,
                        failed: 0,
                        ignored: 0,
                        schemas_created: 8,
                        schemas_updated: 0,
                        errors: Vec::new(),
                    }),
                    None,
                ),
                entry("saas-user", None, Some("HTTP 401：token invalid")),
            ],
            created: 0,
            updated: 2,
            ignored: 0,
            failed: 1,
            schemas_created: 8,
            schemas_updated: 0,
            scanned: "扫描 812 个 Java 文件（映射模块内 240 个）".into(),
        });
        // Target line: project id + name, branch, module, both folders.
        assert!(html.contains("项目ID: 8957701"), "{html}");
        assert!(html.contains("项目名称: ERP"));
        assert!(html.contains("分支/版本: feature"));
        assert!(html.contains("模块: (默认模块)"));
        assert!(html.contains("接口路径: 根目录"));
        assert!(html.contains("数据模型路径: 目录 #12"));
        assert!(html.contains("导入成功"));
        assert!(html.contains("GET /report/profit — 利润报表"));
        // The failed module shows its reason instead of counters.
        assert!(html.contains("⚠ HTTP 401：token invalid"), "{html}");
        // Totals table, as in the reference console.
        assert!(html.contains("Endpoints"));
        assert!(html.contains("Schemas"));
        assert!(html.contains("扫描 812 个 Java 文件"));
        // Balanced markup: the frame shows raw HTML, so a stray tag breaks the panel.
        assert_eq!(html.matches("<div").count(), html.matches("</div>").count(), "{html}");
        assert_eq!(html.matches("<table").count(), html.matches("</table>").count());
        assert_eq!(html.matches("<tr").count(), html.matches("</tr>").count());
    }

    #[test]
    fn failed_branch_lookup_keeps_the_saved_destination() {
        let list: Vec<apifox::Branch> = Vec::new();
        // HTTP 503 for the branch list, row untouched: keep 41/feature.
        assert_eq!(
            resolve_branch(&list, "feature", Some(41), "feature", true).unwrap(),
            (Some(41), "feature".to_string())
        );
        // Same for a row on the main branch.
        assert_eq!(resolve_branch(&list, NO_BRANCH, None, "", true).unwrap(), (None, String::new()));
        // The editor still offers the stored branch instead of showing 主分支.
        let (options, selected) = branch_options(&list, "feature");
        assert!(options.contains(&"feature".to_string()), "{options:?}");
        assert_eq!(selected, "feature");
    }

    #[test]
    fn a_branch_that_cannot_be_validated_is_not_silently_reset() {
        let list: Vec<apifox::Branch> = Vec::new();
        // Another project: the stored branch belongs to the old one, so the
        // save is refused until the list loads.
        assert!(resolve_branch(&list, "feature", Some(41), "feature", false).is_err());
        // A label that is neither stored nor listed is refused too.
        assert!(resolve_branch(&list, "hotfix", Some(41), "feature", true).is_err());
        // Choosing "(主分支)" explicitly is the only way to reset.
        assert_eq!(
            resolve_branch(&list, NO_BRANCH, Some(41), "feature", true).unwrap(),
            (None, String::new())
        );
        // A list that did load wins over the stored id.
        let list = vec![apifox::Branch {
            id: 41,
            name: "feature".into(),
            is_main: false,
            kind: "sprint".into(),
        }];
        assert_eq!(
            resolve_branch(&list, "feature", Some(7), "old", true).unwrap(),
            (Some(41), "feature".to_string())
        );
    }

    #[test]
    fn failed_module_lookup_keeps_the_saved_module_unless_the_branch_changed() {
        let list: Vec<apifox::Module> = Vec::new();
        assert_eq!(
            resolve_module(&list, "orders", Some(7), "orders", true).unwrap(),
            (Some(7), "orders".to_string())
        );
        // Module ids are branch-scoped: after a branch change the old id is
        // not reusable, and the save must ask for a choice.
        assert!(resolve_module(&list, "orders", Some(7), "orders", false).is_err());
        assert_eq!(
            resolve_module(&list, DEFAULT_MODULE, Some(7), "orders", false).unwrap(),
            (None, String::new())
        );
        let list = vec![apifox::Module { id: 7, name: "orders".into() }];
        assert_eq!(
            resolve_module(&list, "orders", Some(1), "old", false).unwrap(),
            (Some(7), "orders".to_string())
        );
    }

    #[test]
    fn preserved_ids_reach_the_next_import_request() {
        // The row as saved before the branch/module lists started failing.
        let mut row = Mapping {
            project_id: "123".into(),
            branch_id: Some(41),
            branch_name: "feature".into(),
            module_id: Some(7),
            module_name: "orders".into(),
            ..Default::default()
        };
        let no_branches: Vec<apifox::Branch> = Vec::new();
        let no_modules: Vec<apifox::Module> = Vec::new();
        // Opening the row and clicking Save while both lookups return 503.
        let (branch_id, branch_name) =
            resolve_branch(&no_branches, "feature", row.branch_id, &row.branch_name, true).unwrap();
        let (module_id, module_name) = resolve_module(
            &no_modules,
            "orders",
            row.module_id,
            &row.module_name,
            branch_id == row.branch_id,
        )
        .unwrap();
        row.branch_id = branch_id;
        row.branch_name = branch_name;
        row.module_id = module_id;
        row.module_name = module_name;

        let opts = apifox::ImportOptions {
            branch_id: row.branch_id,
            module_id: row.module_id,
            ..apifox::ImportOptions::default()
        };
        let request = apifox::import_request(&row.project_id, "{\"openapi\":\"3.0.3\"}", &opts);
        let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
        assert_eq!(body["options"]["targetBranchId"], 41);
        assert_eq!(body["options"]["moduleId"], 7);
    }

    fn saved_row() -> Mapping {
        Mapping {
            project_id: "123".into(),
            project_name: "ERP".into(),
            branch_id: Some(41),
            branch_name: "feature".into(),
            module_id: Some(7),
            module_name: "orders".into(),
            ..Default::default()
        }
    }

    /// Drives the editor through view events, as the IDE does: open the saved
    /// row, type a project id, press Save.
    fn save_after_typing_project(
        project: &str,
        reply: impl Fn(&HttpRequest) -> HttpResponse + 'static,
    ) -> (Option<String>, Vec<Mapping>) {
        let mock = MockHost::install().with_capabilities(&[
            "project.read",
            "project.write",
            "storage",
            "secrets",
            "net:api.apifox.com",
            "net:ask",
        ]);
        mock.set_project_root(Some("/project"));
        mock.set_secret(APIFOX_SECRET, "token");
        mock.on_fetch(move |req| Ok(reply(req)));
        let mut main = Main::default();
        main.mappings = vec![saved_row()];
        main.editing = Some(0);
        main.e_enabled = true;
        main.e_project = "123".into();
        main.e_branch = "feature".into();
        main.e_module = "orders".into();
        // The row as it already sits in storage before the edit.
        save_mappings("/project", &main.mappings).unwrap();
        let ctx = mock.invocation();
        main.handle_editor(0, ViewEvent::Field("e_project".into(), project.into()), "/project", &ctx)
            .unwrap();
        main.handle_editor(0, ViewEvent::Button("save".into()), "/project", &ctx)
            .unwrap();
        (main.status.clone(), load_mappings("/project"))
    }

    fn json_reply(value: &str) -> HttpResponse {
        HttpResponse { status: 200, headers: Vec::new(), body: value.as_bytes().to_vec() }
    }

    #[test]
    fn typing_a_new_project_with_unreachable_lists_keeps_the_old_row() {
        let (status, saved) = save_after_typing_project("456", |_| HttpResponse {
            status: 503,
            headers: Vec::new(),
            body: b"service unavailable".to_vec(),
        });
        // The save is refused, so project 456 never carries 123's ids.
        assert!(status.as_deref().unwrap_or_default().contains("分支列表未加载"), "{status:?}");
        let saved = &saved[0];
        assert_eq!(saved.project_id, "123");
        assert_eq!(saved.branch_id, Some(41));
        assert_eq!(saved.module_id, Some(7));
    }

    #[test]
    fn typing_a_new_project_revalidates_the_destination_against_its_lists() {
        let (status, saved) = save_after_typing_project("456", |req| {
            if req.url.contains("sprint-branches") {
                json_reply(r#"{"success":true,"data":[{"id":61,"name":"feature","type":"sprint"}]}"#)
            } else if req.url.contains("modules") {
                json_reply(r#"{"success":true,"data":[{"id":9,"name":"orders"}]}"#)
            } else {
                json_reply(r#"{"success":true,"data":[]}"#)
            }
        });
        assert_eq!(status.as_deref(), Some("映射已保存"));
        let saved = &saved[0];
        assert_eq!(saved.project_id, "456");
        assert_eq!(saved.branch_id, Some(61));
        assert_eq!(saved.branch_name, "feature");
        assert_eq!(saved.module_id, Some(9));
        assert_eq!(saved.module_name, "orders");
    }

    #[test]
    fn saving_an_untouched_row_through_the_editor_keeps_its_ids() {
        let (status, saved) = save_after_typing_project("123", |_| HttpResponse {
            status: 503,
            headers: Vec::new(),
            body: b"service unavailable".to_vec(),
        });
        assert_eq!(status.as_deref(), Some("映射已保存"));
        let saved = &saved[0];
        assert_eq!(saved.project_id, "123");
        assert_eq!(saved.branch_id, Some(41));
        assert_eq!(saved.module_id, Some(7));
    }

    #[test]
    fn report_escapes_api_text_and_has_an_empty_state() {
        let html = html_of(ImportReport {
            entries: vec![entry("<script>x</script>", None, Some("a & b"))],
            ..Default::default()
        });
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("a &amp; b"));

        let empty = Main::default().report_view();
        assert!(matches!(empty.body, Body::Message(_)));
    }
}